import { describe, expect, it } from "vitest";
import { BROWSE_KEYS, INPUT_KEYS, PREFIX_KEYS, WINDOW_KEYS, resolveKey } from "./keymap";
import type { KeyContext, KeyLike } from "./keymap";

const key = (k: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean; isComposing: boolean }> = {}) =>
  ({ key: k, ctrlKey: false, shiftKey: false, isComposing: false, ...over });
const ctx = { sessionEnded: false };

/** Turns one `BROWSE_KEYS[i].keys` TOKEN (after splitting on " / ") into what a person actually
 *  pressed, per spec §3.3: `Ctrl+x` -> Ctrl held with `x`; `G`/`?` -> Shift held (that is how both
 *  arrive on a real keyboard); `gg` -> a lone `g` with the caller's `pendingG` already true (the
 *  second half of the chord); anything else is the key on its own. Shared by both directions of the
 *  table <-> `resolveKey` correspondence below, so the two can never silently parse a token two
 *  different ways. */
function parseKeyToken(token: string): { ev: KeyLike; pendingG?: boolean } {
  if (token === "gg") return { ev: key("g"), pendingG: true };
  if (token.startsWith("Ctrl+")) return { ev: key(token.slice("Ctrl+".length), { ctrlKey: true }) };
  if (token === "G" || token === "?") return { ev: key(token, { shiftKey: true }) };
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
    expect(resolveKey("browse", key("D", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("U", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("g", { ctrlKey: true }), ctx)).toBeNull();
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

  /* The table keeps no memory: whether a `g` is the second of `gg` is the caller's fact, passed in. */
  it("reads gg from a pending g the caller holds, and a lone g does nothing but ask to be remembered", () => {
    expect(resolveKey("browse", key("g"), ctx)).toEqual({ kind: "pending-g" });
    expect(resolveKey("browse", key("g"), { ...ctx, pendingG: false })).toEqual({ kind: "pending-g" });
    expect(resolveKey("browse", key("g"), { ...ctx, pendingG: true })).toEqual({ kind: "jump", to: "first" });
    // A pending g changes nothing about any other key.
    expect(resolveKey("browse", key("j"), { ...ctx, pendingG: true })).toEqual({ kind: "move", delta: 1 });
    expect(resolveKey("input", key("g"), { ...ctx, pendingG: true })).toBeNull();
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
});

/* The two-way check spec §3.3 asks for: `BROWSE_KEYS` may neither promise a key `resolveKey` does
   nothing with, nor leave out a key that does something. Each half is its own `it` so a failure
   names which direction broke. */
describe("BROWSE_KEYS <-> resolveKey", () => {
  it("every key BROWSE_KEYS lists actually does something in resolveKey", () => {
    for (const { keys } of BROWSE_KEYS) {
      for (const token of keys.split(" / ")) {
        const { ev, pendingG } = parseKeyToken(token);
        const localCtx: KeyContext = { sessionEnded: token === "r", pendingG };
        expect(resolveKey("browse", ev, localCtx), `"${token}" (from "${keys}")`).not.toBeNull();
      }
    }
  });

  /* Every candidate a person could plausibly press, checked under both endings of a session (some
     rows, like `a`/`d`/`i`, only resolve on one side of that flag). `pending-g` is excluded on
     purpose: it is the first half of `gg`, which the table already lists as `gg`, not as a bare `g`
     -- see the comment on `PanelAction`'s `pending-g` case. Deleting the `y` row (spec's own red
     check for this test) makes "y" resolve to `{kind:"copy"}` here with no row to match it against. */
  it("every key that does something in resolveKey is listed somewhere in BROWSE_KEYS", () => {
    const known = new Set(BROWSE_KEYS.flatMap((row) => row.keys.split(" / ")));
    const candidates: { token: string; ev: KeyLike; pendingG?: boolean }[] = [
      ...("abcdefghijklmnopqrstuvwxyz".split("").map((letter) => ({ token: letter, ev: key(letter) }))),
      { token: "Enter", ev: key("Enter") },
      { token: "Escape", ev: key("Escape") },
      { token: "?", ev: key("?", { shiftKey: true }) },
      { token: "G", ev: key("G", { shiftKey: true }) },
      { token: "Ctrl+d", ev: key("d", { ctrlKey: true }) },
      { token: "Ctrl+u", ev: key("u", { ctrlKey: true }) },
      { token: "gg", ev: key("g"), pendingG: true },
    ];
    for (const sessionEnded of [false, true]) {
      for (const { token, ev, pendingG } of candidates) {
        const result = resolveKey("browse", ev, { sessionEnded, pendingG });
        if (result === null || result.kind === "pending-g") continue;
        expect(known.has(token), `"${token}" (sessionEnded=${sessionEnded}) resolves to ${JSON.stringify(result)} but no BROWSE_KEYS row spells it`).toBe(true);
      }
    }
  });
});

describe("the ? keymap tables", () => {
  it("are each non-empty and free of duplicate keys", () => {
    for (const table of [BROWSE_KEYS, INPUT_KEYS, WINDOW_KEYS, PREFIX_KEYS]) {
      expect(table.length).toBeGreaterThan(0);
      expect(new Set(table.map((row) => row.keys)).size).toBe(table.length);
    }
  });
});
