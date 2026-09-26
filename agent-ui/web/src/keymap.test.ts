import { describe, expect, it } from "vitest";
import { BROWSE_KEYS, INPUT_KEYS, resolveKey } from "./keymap";
import type { KeyContext, KeyLike, PanelBinding, PanelTable, PendingPrefix } from "./keymap";

const key = (k: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean; isComposing: boolean; keyCode: number }> = {}) =>
  ({ key: k, ctrlKey: false, shiftKey: false, isComposing: false, ...over });
const ctx = { sessionEnded: false };

/** Turns one `BROWSE_KEYS[i].keys` TOKEN (after splitting on " / ") into what a person actually
 *  pressed, per spec §3.3: a two-key sequence (`gg`, `[[`, `]]`) is its second key with the first
 *  held as `pending`; `Ctrl+x` -> Ctrl held with `x`; `G`/`?` -> Shift held (that is how both arrive
 *  on a real keyboard); `1-9` -> a single representative digit; anything else is the key on its own.
 *  Shared by both directions of the table <-> `resolveKey` correspondence below, so the two can
 *  never silently parse a token two different ways. */
function parseKeyToken(token: string): { ev: KeyLike; pending?: PendingPrefix } {
  if (/^[gz\[\]].$/.test(token)) return { ev: key(token[1]), pending: token[0] as PendingPrefix };
  if (token === "1-9") return { ev: key("3") };
  if (token.startsWith("Ctrl+")) return { ev: key(token.slice("Ctrl+".length), { ctrlKey: true }) };
  if (/^[A-Z?]$/.test(token)) return { ev: key(token, { shiftKey: true }) };
  return { ev: key(token) };
}

describe("resolveKey", () => {
  it("enters INPUT on i and leaves it on Esc", () => {
    expect(resolveKey("browse", key("i"), ctx)).toEqual({ kind: "mode", to: "input" });
    expect(resolveKey("input", key("Escape"), ctx)).toEqual({ kind: "mode", to: "browse" });
  });

  it("gives Esc back to the input method while composing", () => {
    expect(resolveKey("input", key("Escape", { isComposing: true }), ctx)).toBeNull();
  });

  it("moves between stops with j and k, and between a stop's controls with h and l", () => {
    // Which stop or control that is depends on the document, so the table only names a direction.
    // The clamping at both ends lives in `./nav` and is tested there.
    expect(resolveKey("browse", key("j"), ctx)).toEqual({ kind: "move", delta: 1 });
    expect(resolveKey("browse", key("k"), ctx)).toEqual({ kind: "move", delta: -1 });
    expect(resolveKey("browse", key("l"), ctx)).toEqual({ kind: "control", delta: 1 });
    expect(resolveKey("browse", key("h"), ctx)).toEqual({ kind: "control", delta: -1 });
  });

  it("answers a permission with a and d, but never on a session that ended", () => {
    expect(resolveKey("browse", key("a"), ctx)).toEqual({ kind: "answer", decision: "allow" });
    expect(resolveKey("browse", key("d"), ctx)).toEqual({ kind: "answer", decision: "deny" });
    expect(resolveKey("browse", key("a"), { sessionEnded: true })).toBeNull();
    expect(resolveKey("browse", key("d"), { sessionEnded: true })).toBeNull();
    // Typing an `a` into the composer is typing an `a`.
    expect(resolveKey("input", key("a"), ctx)).toBeNull();
  });

  it("offers restart only on a session that ended", () => {
    expect(resolveKey("browse", key("r"), ctx)).toBeNull();
    expect(resolveKey("browse", key("r"), { sessionEnded: true })).toEqual({ kind: "restart" });
  });

  /* The other direction of the same flag, and the reason it is one flag: INPUT on a dead session
     was a one-way trap. The composer's textarea is `disabled` there, so `autoFocus` cannot take
     focus and keys keep arriving at the panel root -- where the "input" branch resolves nothing but
     `Escape`, dropping `r`/`j`/`k`/`y` while the lost-session row kept printing "Press r to return
     to the start screen." Entering a mode that drops the key the screen promises is what this
     refuses; `Composer` stops showing its focusable placeholder on the same condition, so nothing on
     screen promises `i` either. */
  it("refuses i on a session that ended, because INPUT there has no box and drops r", () => {
    expect(resolveKey("browse", key("i"), ctx)).toEqual({ kind: "mode", to: "input" });
    expect(resolveKey("browse", key("i"), { sessionEnded: true })).toBeNull();
    // Still nothing but Escape once in INPUT -- which is why the entrance is what had to close.
    expect(resolveKey("input", key("r"), { sessionEnded: true })).toBeNull();
  });

  it("ignores browse keys while typing", () => {
    expect(resolveKey("input", key("j"), ctx)).toBeNull();
    expect(resolveKey("input", key("y"), ctx)).toBeNull();
  });

  /* Review round 1, item 5: `KeyLike` has always declared `ctrlKey`/`shiftKey`; nothing read them,
     which is indistinguishable from a modifier check that silently does not exist. A held Ctrl or
     Shift now means "not this table's key" across every row, not just the ones it happens to name. */
  it("does not claim a key carrying a modifier this table does not name", () => {
    expect(resolveKey("browse", key("i", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("j", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("y", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("r", { ctrlKey: true }), { sessionEnded: true })).toBeNull();
    expect(resolveKey("input", key("Escape", { shiftKey: true }), ctx)).toBeNull();
    // The three chords it DOES name (below) are named exactly: every other modifier set on the
    // same letters, and every other Ctrl/Shift letter a vim user might reach for, is still refused.
    expect(resolveKey("browse", key("d", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("U", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("G", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("f", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("b", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("e", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("J", { shiftKey: true }), ctx)).toBeNull();
    // And the named ones belong to BROWSE only: in the composer they are the text box's own.
    expect(resolveKey("input", key("d", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("input", key("u", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("input", key("G", { shiftKey: true }), ctx)).toBeNull();
  });

  /* Checked ahead of the plain-key switch: a browser reports `key === "d"` for Ctrl+d, so a Ctrl+d
     that reached `case "d"` would DENY a pending permission. */
  it("names Ctrl+d, Ctrl+u and Shift+G, and a Ctrl+d is never a deny", () => {
    expect(resolveKey("browse", key("d", { ctrlKey: true }), ctx)).toEqual({ kind: "half-page", delta: 1 });
    expect(resolveKey("browse", key("u", { ctrlKey: true }), ctx)).toEqual({ kind: "half-page", delta: -1 });
    expect(resolveKey("browse", key("G", { shiftKey: true }), ctx)).toEqual({ kind: "jump", to: "last" });
    expect(resolveKey("browse", key("d"), ctx)).toEqual({ kind: "answer", decision: "deny" });
    // Scrolling a dead session's transcript is still reading it.
    expect(resolveKey("browse", key("d", { ctrlKey: true }), { sessionEnded: true })).toEqual({ kind: "half-page", delta: 1 });
  });

  it("names Shift+Y and Shift+D ahead of the modifier refusal", () => {
    expect(resolveKey("browse", key("Y", { shiftKey: true }), ctx)).toEqual({ kind: "copy-output" });
    expect(resolveKey("browse", key("D", { shiftKey: true }), ctx)).toEqual({ kind: "deny-reason" });
    expect(resolveKey("browse", key("D", { shiftKey: true }), { sessionEnded: true })).toBeNull();
  });

  /* The table keeps no memory: whether a `g` is the second of `gg` is the caller's fact, passed in. */
  it("reads gg from a pending g the caller holds, and a lone g does nothing but ask to be remembered", () => {
    expect(resolveKey("browse", key("g"), ctx)).toEqual({ kind: "pending", prefix: "g" });
    expect(resolveKey("browse", key("g"), { ...ctx, pending: null })).toEqual({ kind: "pending", prefix: "g" });
    expect(resolveKey("browse", key("g"), { ...ctx, pending: "g" })).toEqual({ kind: "jump", to: "first" });
    // A pending g changes nothing about any other key.
    expect(resolveKey("browse", key("j"), { ...ctx, pending: "g" })).toEqual({ kind: "move", delta: 1 });
    expect(resolveKey("input", key("g"), { ...ctx, pending: "g" })).toBeNull();
  });

  it("starts a global HINT on f in BROWSE, and does not in INPUT", () => {
    expect(resolveKey("browse", key("f"), ctx)).toEqual({ kind: "hint" });
    expect(resolveKey("input", key("f"), ctx)).toBeNull();
    // Ctrl+Shift+F is shell's app-level accelerator (GTK takes it first); a Ctrl+f that does reach
    // the page is not claimed here, and neither is a Shift+F.
    expect(resolveKey("browse", key("f", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("F", { shiftKey: true }), ctx)).toBeNull();
  });

  /* `?` almost always arrives as Shift+/ (spec §3.1), so it has to be named ahead of the blanket
     modifier refusal the same way `G` is -- checked with Shift both ways so a regression that moved
     it below that refusal, or that started requiring Shift, is caught either direction. */
  it("opens the ? keymap in BROWSE with or without Shift, and never in INPUT", () => {
    expect(resolveKey("browse", key("?", { shiftKey: true }), ctx)).toEqual({ kind: "keymap" });
    expect(resolveKey("browse", key("?", { shiftKey: false }), ctx)).toEqual({ kind: "keymap" });
    expect(resolveKey("input", key("?", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("input", key("?", { shiftKey: false }), ctx)).toBeNull();
  });

  it("opens the row's path on gf and views the row in nvim on Ctrl+g", () => {
    expect(resolveKey("browse", key("f"), { ...ctx, pending: "g" })).toEqual({ kind: "open-path" });
    expect(resolveKey("browse", key("f"), ctx), "a lone f is still HINT").toEqual({ kind: "hint" });
    expect(resolveKey("browse", key("g", { ctrlKey: true }), ctx)).toEqual({ kind: "view-in-editor" });
    expect(resolveKey("input", key("g", { ctrlKey: true }), ctx), "INPUT's Ctrl+g is the composer's").toBeNull();
  });
});

describe("resolveKey, phase 3", () => {
  it("reads two-key sequences from the prefix the caller holds", () => {
    expect(resolveKey("browse", key("g"), ctx)).toEqual({ kind: "pending", prefix: "g" });
    expect(resolveKey("browse", key("g"), { ...ctx, pending: "g" })).toEqual({ kind: "jump", to: "first" });
    expect(resolveKey("browse", key("["), ctx)).toEqual({ kind: "pending", prefix: "[" });
    expect(resolveKey("browse", key("["), { ...ctx, pending: "[" })).toEqual({ kind: "prompt-jump", delta: -1 });
    expect(resolveKey("browse", key("]"), { ...ctx, pending: "]" })).toEqual({ kind: "prompt-jump", delta: 1 });
    expect(resolveKey("browse", key("]"), { ...ctx, pending: "[" }), "a mismatched pair is nothing").toBeNull();
    expect(resolveKey("browse", key("j"), { ...ctx, pending: "[" }), "any other key cancels the prefix").toEqual({ kind: "move", delta: 1 });
  });

  it("collects a count from 1-9, then 0-9", () => {
    expect(resolveKey("browse", key("3"), ctx)).toEqual({ kind: "count", digit: 3 });
    expect(resolveKey("browse", key("0"), ctx), "0 starts no count").toBeNull();
    expect(resolveKey("browse", key("0"), { ...ctx, count: 1 })).toEqual({ kind: "count", digit: 0 });
    expect(resolveKey("input", key("3"), ctx)).toBeNull();
  });

  it("interrupts on Ctrl+c only while a turn runs, and never exits (D1, N1)", () => {
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, turnRunning: true })).toEqual({ kind: "interrupt" });
    expect(resolveKey("browse", key("c", { ctrlKey: true }), ctx), "idle: native copy").toBeNull();
    expect(resolveKey("browse", key("d", { ctrlKey: true }), ctx), "Ctrl+d is still half a page, never exit").toEqual({ kind: "half-page", delta: 1 });
  });

  it("never interrupts or denies on Esc (D1)", () => {
    expect(resolveKey("browse", key("Escape"), { ...ctx, turnRunning: true })).toBeNull();
  });

  it("opens a search on / and repeats it with n and Shift+N", () => {
    expect(resolveKey("browse", key("/"), ctx)).toEqual({ kind: "search" });
    expect(resolveKey("browse", key("n"), ctx)).toEqual({ kind: "search-next", delta: 1 });
    expect(resolveKey("browse", key("N", { shiftKey: true }), ctx)).toEqual({ kind: "search-next", delta: -1 });
    expect(resolveKey("input", key("/"), ctx)).toBeNull();
  });

  it("toggles the detailed view on Ctrl+o in both modes, and scrolls a table on zh / zl", () => {
    expect(resolveKey("browse", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "detailed" });
    expect(resolveKey("input", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "detailed" });
    expect(resolveKey("browse", key("z"), ctx)).toEqual({ kind: "pending", prefix: "z" });
    expect(resolveKey("browse", key("h"), { ...ctx, pending: "z" })).toEqual({ kind: "table-scroll", delta: -1 });
    expect(resolveKey("browse", key("l"), { ...ctx, pending: "z" })).toEqual({ kind: "table-scroll", delta: 1 });
  });

  it("gives every key to the input method while it composes, in BROWSE too (C4)", () => {
    for (const k of ["a", "d", "j", "Enter", "3"]) {
      expect(resolveKey("browse", key(k, { isComposing: true }), ctx), k).toBeNull();
      expect(resolveKey("browse", key(k, { keyCode: 229 }), ctx), k).toBeNull();
    }
  });
});

/** Just enough of a `PanelTable` (panel round 2 plan, Task 7) to exercise `resolveKey`'s
 *  second-key-of-a-pending-prefix lookup: one binding, `[b`, that shares its first key with the
 *  fixed `[[` prompt-jump pair. */
const TABLE: PanelTable = {
  leader: " ",
  leaderLabel: "Space",
  leaderSource: "default",
  timeoutlen: 1000,
  timeout: true,
  bindings: [{ keys: ["[", "b"], action: "tab.prev", desc: "previous tab", source: "default" }],
  groups: [],
};

describe("resolveKey with a panel table (panel round 2 plan, Task 7)", () => {
  it("resolves a second key the fixed pairs don't own to the table's binding", () => {
    const binding: PanelBinding = TABLE.bindings[0];
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "panel", binding });
  });
  it("still runs the fixed [[ prompt-jump even with a table present", () => {
    expect(resolveKey("browse", key("["), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "prompt-jump", delta: -1 });
  });
  it("resolves to null for [b without a table, exactly as today", () => {
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[" })).toBeNull();
  });
});

/* The two-way check spec §3.3 asks for: `BROWSE_KEYS` may neither promise a key `resolveKey` does
   nothing with, nor leave out a key that does something. Each half is its own `it` so a failure
   names which direction broke. */
describe("BROWSE_KEYS <-> resolveKey", () => {
  it("every key BROWSE_KEYS lists actually does something in resolveKey", () => {
    for (const { keys } of BROWSE_KEYS) {
      for (const token of keys.split(" / ")) {
        const { ev, pending } = parseKeyToken(token);
        // `turnRunning: true` so `Ctrl+c`'s own row -- idle it resolves to nothing at all -- still
        // proves it does something, the same reason `sessionEnded` is set for `r`'s row below.
        const localCtx: KeyContext = { sessionEnded: token === "r", pending, turnRunning: true };
        expect(resolveKey("browse", ev, localCtx), `"${token}" (from "${keys}")`).not.toBeNull();
      }
    }
  });

  /* Every candidate a person could plausibly press, checked under both endings of a session (some
     rows, like `a`/`d`/`i`, only resolve on one side of that flag). A lone `g`/digit is excluded on
     purpose: it is the first half of `gg`, or the start of a count the table spells `1-9`/`gg`, not
     as itself -- see `PanelAction`'s `pending`/`count` cases. Deleting the `y` row (spec's own red
     check for this test) makes "y" resolve to `{kind:"copy"}` here with no row to match it against. */
  it("every key that does something in resolveKey is listed somewhere in BROWSE_KEYS", () => {
    const known = new Set(BROWSE_KEYS.flatMap((row) => row.keys.split(" / ")));
    const candidates: { token: string; ev: KeyLike; pending?: PendingPrefix; table?: PanelTable }[] = [
      ...("abcdefghijklmnopqrstuvwxyz".split("").map((letter) => ({ token: letter, ev: key(letter) }))),
      { token: "Enter", ev: key("Enter") },
      { token: "Escape", ev: key("Escape") },
      { token: "?", ev: key("?", { shiftKey: true }) },
      { token: "G", ev: key("G", { shiftKey: true }) },
      { token: "Y", ev: key("Y", { shiftKey: true }) },
      { token: "D", ev: key("D", { shiftKey: true }) },
      { token: "Ctrl+d", ev: key("d", { ctrlKey: true }) },
      { token: "Ctrl+u", ev: key("u", { ctrlKey: true }) },
      { token: "gg", ev: key("g"), pending: "g" },
      { token: "1-9", ev: key("3") },
      { token: "[[", ev: key("["), pending: "[" },
      { token: "]]", ev: key("]"), pending: "]" },
      { token: "Ctrl+c", ev: key("c", { ctrlKey: true }) },
      { token: "/", ev: key("/") },
      { token: "N", ev: key("N", { shiftKey: true }) },
      { token: "Ctrl+o", ev: key("o", { ctrlKey: true }) },
      { token: "zh", ev: key("h"), pending: "z" },
      { token: "zl", ev: key("l"), pending: "z" },
      { token: "gf", ev: key("f"), pending: "g" },
      { token: "Ctrl+g", ev: key("g", { ctrlKey: true }) },
      // A key named only by the panel's own which-key table (Task 7): the overlay's new section
      // (Task 8) names it, not BROWSE_KEYS, so it must resolve without needing a row here.
      { token: "[b", ev: key("b"), pending: "[", table: TABLE },
    ];
    for (const sessionEnded of [false, true]) {
      for (const { token, ev, pending, table } of candidates) {
        const result = resolveKey("browse", ev, { sessionEnded, pending, table, turnRunning: true });
        if (result === null) continue;
        // `pending`/`count` are intermediate results -- claimed, but not yet the thing the key does.
        // A candidate the table spells explicitly (`1-9`, `[[`, `]]`) is checked like everything
        // else; a bare letter or digit that merely STARTS one of those (a lone `g`, `[`, `]`, `3`)
        // is skipped, the same as `gg`'s own first `g` always was.
        if ((result.kind === "pending" || result.kind === "count") && !known.has(token)) continue;
        // A table-resolved key is listed by the overlay's own panel section (Task 8), never here.
        if (result.kind === "panel") continue;
        expect(known.has(token), `"${token}" (sessionEnded=${sessionEnded}) resolves to ${JSON.stringify(result)} but no BROWSE_KEYS row spells it`).toBe(true);
      }
    }
  });
});

describe("the ? keymap tables", () => {
  it("are each non-empty and free of duplicate keys", () => {
    for (const table of [BROWSE_KEYS, INPUT_KEYS]) {
      expect(table.length).toBeGreaterThan(0);
      expect(new Set(table.map((row) => row.keys)).size).toBe(table.length);
    }
  });
});
