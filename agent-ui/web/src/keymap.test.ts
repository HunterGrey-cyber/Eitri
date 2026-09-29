import { describe, expect, it } from "vitest";
import { noteKey } from "./heldSuper";
import { BROWSE_KEYS, CARET_KEYS, INPUT_KEYS, VISUAL_KEYS, isPlainAnswerKey, resolveKey } from "./keymap";
import type { KeyContext, KeyLike, PanelBinding, PanelTable, PendingPrefix } from "./keymap";

const key = (
  k: string,
  over: Partial<{
    ctrlKey: boolean;
    shiftKey: boolean;
    isComposing: boolean;
    keyCode: number;
    altKey: boolean;
    metaKey: boolean;
    /** Creates a KeyLike whose getModifierState reports Super and Hyper held, matching the
     *  modifier-query interface of KeyboardEvent. */
    superKey: boolean;
    /** `superKey` reports Super and Hyper together, so it cannot isolate a Hyper-only failure.
     *  This reports Hyper alone, as a Hyper-only keypress would. */
    hyperKey: boolean;
    /** Creates a KeyLike whose getModifierState reports AltGraph held. This is the level-3
     *  shift some layouts use to type ordinary characters such as @ and €. */
    altGraphKey: boolean;
  }> = {},
) => {
  const { superKey, hyperKey, altGraphKey, ...rest } = over;
  return {
    key: k,
    ctrlKey: false,
    shiftKey: false,
    isComposing: false,
    ...rest,
    ...(superKey ? { getModifierState: (m: string) => m === "Super" || m === "Hyper" } : {}),
    ...(hyperKey ? { getModifierState: (m: string) => m === "Hyper" } : {}),
    ...(altGraphKey ? { getModifierState: (m: string) => m === "AltGraph" } : {}),
  };
};
const ctx = { sessionEnded: false };

/** Parses one BROWSE_KEYS[i].keys token after splitting on " / ". A two-key sequence such as
 *  gg, [[ or ]] uses its second key with the first stored as pending. Ctrl+x carries Ctrl, G
 *  and ? carry Shift, and 1-9 uses a representative digit. Other tokens are keys on their own.
 *  Both directions of the table-to-resolveKey checks share this parser. */
function parseKeyToken(token: string): { ev: KeyLike; pending?: PendingPrefix } {
  if (/^[gz\[\]].$/.test(token)) return { ev: key(token[1]), pending: token[0] as PendingPrefix };
  if (token === "1-9") return { ev: key("3") };
  if (token.startsWith("Ctrl+")) return { ev: key(token.slice("Ctrl+".length), { ctrlKey: true }) };
  if (/^[A-Z?]$/.test(token)) return { ev: key(token, { shiftKey: true }) };
  return { ev: key(token) };
}

describe("resolveKey", () => {
  it("enters INPUT on i and leaves it on Esc", () => {
    expect(resolveKey("browse", key("i"), ctx)).toEqual({ kind: "mode", to: "input", caret: "kept" });
    expect(resolveKey("input", key("Escape"), ctx)).toEqual({ kind: "mode", to: "browse" });
  });

  /* C1a: `o` is an exact alias of `i` -- same caret, same refusal -- and `A` is `i`'s
     shifted sibling with the caret forced to the end (`:h A`). Neither reaches `O`/`I`, which stay
     unbound (additive later): the blanket Shift refusal still catches them. */
  it("adds o as an exact alias of i, and A at the end of the draft (C1a)", () => {
    expect(resolveKey("browse", key("o"), ctx)).toEqual({ kind: "mode", to: "input", caret: "kept" });
    expect(resolveKey("browse", key("A", { shiftKey: true }), ctx)).toEqual({ kind: "mode", to: "input", caret: "end" });
    expect(resolveKey("browse", key("o"), { sessionEnded: true })).toBeNull();
    expect(resolveKey("browse", key("A", { shiftKey: true }), { sessionEnded: true })).toBeNull();
    expect(resolveKey("browse", key("O", { shiftKey: true }), ctx), "O stays unbound").toBeNull();
    expect(resolveKey("browse", key("I", { shiftKey: true }), ctx), "I stays unbound").toBeNull();
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

  /* Alt or Meta held with a card-answer key must not authorize or deny a card: OS shortcuts
     such as Meta+A must not trigger permission answers. Bare a/d must still work. Super is a
     separate GDK modifier and needs its own check. */
  it("refuses Alt or Meta on a/d, but bare a/d still answer (R2)", () => {
    expect(resolveKey("browse", key("a", { altKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("a", { metaKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("d", { altKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("d", { metaKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("a"), ctx)).toEqual({ kind: "answer", decision: "allow" });
    expect(resolveKey("browse", key("d"), ctx)).toEqual({ kind: "answer", decision: "deny" });
  });

  /* Checking altKey and metaKey alone misses Super and Hyper. This pure layer queries
     getModifierState("Super"/"Hyper") to refuse modified card-answer keys. Whether WebKitGTK
     supplies that state on another key's event must be checked separately. */
  it("refuses Super (or Hyper) on a/d, but bare a/d still answer", () => {
    expect(resolveKey("browse", key("a", { superKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("d", { superKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("a"), ctx)).toEqual({ kind: "answer", decision: "allow" });
    expect(resolveKey("browse", key("d"), ctx)).toEqual({ kind: "answer", decision: "deny" });
  });

  /* The sandbox pass: WebKitGTK 2.52.6 sets no modifier on `a` while Super is
     held -- only Super's own keydown/keyup arrive, which `heldSuper.ts` tracks. */
  it("refuses a/d while Super is held by its own keydown, with no modifier on the key itself", () => {
    noteKey("keydown", { key: "Super", code: "OSLeft" });
    try {
      expect(resolveKey("browse", key("a"), ctx)).toBeNull();
      expect(resolveKey("browse", key("d"), ctx)).toBeNull();
    } finally {
      noteKey("keyup", { key: "Super", code: "OSLeft" });
    }
    expect(resolveKey("browse", key("a"), ctx)).toEqual({ kind: "answer", decision: "allow" });
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
    expect(resolveKey("browse", key("i"), ctx)).toEqual({ kind: "mode", to: "input", caret: "kept" });
    expect(resolveKey("browse", key("i"), { sessionEnded: true })).toBeNull();
    // Still nothing but Escape once in INPUT -- which is why the entrance is what had to close.
    expect(resolveKey("input", key("r"), { sessionEnded: true })).toBeNull();
  });

  it("ignores browse keys while typing", () => {
    expect(resolveKey("input", key("j"), ctx)).toBeNull();
    expect(resolveKey("input", key("y"), ctx)).toBeNull();
  });

  /* A held Ctrl or Shift must prevent a match unless the binding names that modifier. Checking
     every row ensures an unnamed modifier cannot silently trigger an ordinary key action. */
  it("does not claim a key carrying a modifier this table does not name", () => {
    expect(resolveKey("browse", key("i", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("j", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("y", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("r", { ctrlKey: true }), { sessionEnded: true })).toBeNull();
    expect(resolveKey("input", key("Escape", { shiftKey: true }), ctx)).toBeNull();
    // The chords it DOES name (below, and Ctrl+e/Ctrl+y -- their own test just below this one) are
    // named exactly: every other modifier set on the same letters, and every other Ctrl/Shift
    // letter a vim user might reach for, is still refused.
    expect(resolveKey("browse", key("d", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    // Ctrl+a reaches the blanket modifier refusal before `isPlainAnswerKey` is consulted.
    // This checks Ctrl separately from the Alt, Meta and Super cases above.
    expect(resolveKey("browse", key("a", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("U", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("G", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("f", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("b", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("p", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("J", { shiftKey: true }), ctx)).toBeNull();
    // Ctrl+Shift+e/y are refused, like Ctrl+Shift+d above; only the unshifted Ctrl+e/y chords scroll.
    expect(resolveKey("browse", key("e", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("y", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
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

  /* Check Ctrl+e/Ctrl+y before the plain-key switch. A browser reports `key === "y"` for both
     Ctrl+y and bare `y`, so letting Ctrl+y reach the switch would copy the current row. */
  it("names Ctrl+e and Ctrl+y as one-line scrolls, and a Ctrl+y is never a copy", () => {
    expect(resolveKey("browse", key("e", { ctrlKey: true }), ctx)).toEqual({ kind: "scroll-line", delta: 1 });
    expect(resolveKey("browse", key("y", { ctrlKey: true }), ctx)).toEqual({ kind: "scroll-line", delta: -1 });
    expect(resolveKey("browse", key("y"), ctx)).toEqual({ kind: "copy" });
    // Scrolling a dead session's transcript is still reading it, the same as Ctrl+d/Ctrl+u.
    expect(resolveKey("browse", key("e", { ctrlKey: true }), { sessionEnded: true })).toEqual({ kind: "scroll-line", delta: 1 });
    // BROWSE only: INPUT's own Ctrl+e (end of line) and Ctrl+y are the composer's.
    expect(resolveKey("input", key("e", { ctrlKey: true }), ctx)).toBeNull();
    expect(resolveKey("input", key("y", { ctrlKey: true }), ctx)).toBeNull();
  });

  it("names Shift+Y and Shift+D ahead of the modifier refusal", () => {
    expect(resolveKey("browse", key("Y", { shiftKey: true }), ctx)).toEqual({ kind: "copy-output" });
    expect(resolveKey("browse", key("D", { shiftKey: true }), ctx)).toEqual({ kind: "deny-reason" });
    expect(resolveKey("browse", key("D", { shiftKey: true }), { sessionEnded: true })).toBeNull();
  });

  /* Shift+D is checked before the blanket modifier refusal, so Alt and Meta need their
     own guard on this chord. */
  it("refuses Alt or Meta on Shift+D (R2)", () => {
    expect(resolveKey("browse", key("D", { shiftKey: true, altKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("D", { shiftKey: true, metaKey: true }), ctx)).toBeNull();
  });

  /* Shift+D needs its own Super/Hyper guard because it bypasses the blanket modifier refusal. */
  it("refuses Super (or Hyper) on Shift+D", () => {
    expect(resolveKey("browse", key("D", { shiftKey: true, superKey: true }), ctx)).toBeNull();
  });

  /* Card answers and bypass confirmation share `isPlainAnswerKey`. The a/d/D paths also reject
     composing input at the start of `resolveKey`, before any chord or switch is checked.
     Both `isComposing` and legacy WebKit's `keyCode === 229` must block answers, matching
     `composerKeys.ts`'s `isImeKey`. */
  it("refuses a/d/D while an input method is composing (C4, reconciled with the bypass y fix)", () => {
    expect(resolveKey("browse", key("a", { isComposing: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("d", { isComposing: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("D", { shiftKey: true, isComposing: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("a", { keyCode: 229 }), ctx)).toBeNull();
    expect(resolveKey("browse", key("d", { keyCode: 229 }), ctx)).toBeNull();
    expect(resolveKey("browse", key("D", { shiftKey: true, keyCode: 229 }), ctx)).toBeNull();
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

  /* `?` almost always arrives as Shift+/, so it has to be named ahead of the blanket
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

  it("interrupts on Ctrl+c only while a turn runs, and never exits", () => {
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, turnRunning: true })).toEqual({ kind: "interrupt" });
    expect(resolveKey("browse", key("c", { ctrlKey: true }), ctx), "idle: native copy").toBeNull();
    expect(resolveKey("browse", key("d", { ctrlKey: true }), ctx), "Ctrl+d is still half a page, never exit").toEqual({ kind: "half-page", delta: 1 });
  });

  it("never interrupts or denies on Esc, and flashes rather than acting while a turn runs", () => {
    expect(resolveKey("browse", key("Escape"), { ...ctx, turnRunning: true })).toEqual({ kind: "esc-blocked" });
    expect(resolveKey("browse", key("Escape"), ctx), "idle: unclaimed, exactly as before").toBeNull();
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

/** Just enough of a `PanelTable` to exercise `resolveKey`'s
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

describe("resolveKey with a panel table", () => {
  it("resolves a second key the fixed pairs don't own to the table's binding", () => {
    const binding: PanelBinding = TABLE.bindings[0];
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "panel", binding });
  });
  it("still runs the fixed [[ prompt-jump even with a table present", () => {
    expect(resolveKey("browse", key("["), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "prompt-jump", delta: -1 });
  });
  /** `gT` (`:help gT`) is typed with Shift held; the pair lookup runs ahead of the
   *  blanket modifier refusal, but a Ctrl chord never completes a pair. */
  it("resolves g then Shift+T to the table's gT, but not g then Ctrl+T", () => {
    const gT: PanelBinding = { keys: ["g", "T"], action: "tab.prev", desc: "previous tab", source: "default" };
    const table: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, gT] };
    expect(resolveKey("browse", key("T", { shiftKey: true }), { ...ctx, pending: "g", table })).toEqual({ kind: "panel", binding: gT });
    expect(resolveKey("browse", key("T", { ctrlKey: true, shiftKey: true }), { ...ctx, pending: "g", table })).toBeNull();
    expect(resolveKey("input", key("T", { shiftKey: true }), { ...ctx, pending: "g", table })).toBeNull();
  });
  it("resolves to null for [b without a table, exactly as today", () => {
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[" })).toBeNull();
  });
});

/* `BROWSE_KEYS` must list every key `resolveKey` handles and no key it ignores.
   Separate tests identify which direction of the check failed. */
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
      { token: "A", ev: key("A", { shiftKey: true }) },
      { token: "Ctrl+d", ev: key("d", { ctrlKey: true }) },
      { token: "Ctrl+u", ev: key("u", { ctrlKey: true }) },
      { token: "Ctrl+e", ev: key("e", { ctrlKey: true }) },
      { token: "Ctrl+y", ev: key("y", { ctrlKey: true }) },
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
      // A key named only by the panel's own which-key table: the overlay's new section
      // names it, not BROWSE_KEYS, so it must resolve without needing a row here.
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
        // A table-resolved key is listed by the overlay's own panel section, never here.
        if (result.kind === "panel") continue;
        // Esc only flashes a reminder while a reply is running; it performs no action to list in
        // the `?` overlay. `turnRunning: true` is what makes this result reachable here.
        if (result.kind === "esc-blocked") continue;
        expect(known.has(token), `"${token}" (sessionEnded=${sessionEnded}) resolves to ${JSON.stringify(result)} but no BROWSE_KEYS row spells it`).toBe(true);
      }
    }
  });
});

/** VISUAL/V-LINE's own token parser, mirroring `parseKeyToken` above but for the shape `VISUAL_KEYS`
 *  rows actually take: no two-key sequences, `"1-9"` a representative digit, `"Esc"` the display
 *  spelling of the real `key: "Escape"`, and every bare uppercase letter or `?` carrying Shift, the
 *  same reason `parseKeyToken` does. */
function parseVisualToken(token: string): KeyLike {
  if (token === "Esc") return key("Escape");
  if (token === "1-9") return key("3");
  // For `gg`, the table tests only need the first press, which arms a pending marker.
  // Separate tests check the complete sequence alongside `G`.
  if (token === "gg") return key("g");
  if (/^[A-Z?]$/.test(token)) return key(token, { shiftKey: true });
  return key(token);
}

describe("VISUAL_KEYS <-> resolveKey", () => {
  it("every key VISUAL_KEYS lists actually does something in either VISUAL or V-LINE", () => {
    for (const { keys } of VISUAL_KEYS) {
      for (const token of keys.split(" / ")) {
        const ev = parseVisualToken(token);
        expect(resolveKey("visual", ev, ctx), `"${token}" (from "${keys}") in visual`).not.toBeNull();
        expect(resolveKey("vline", ev, ctx), `"${token}" (from "${keys}") in vline`).not.toBeNull();
      }
    }
  });

  /* An unbound plain letter in VISUAL ends the region as `vend`, so check that every letter
     absent from `VISUAL_KEYS` does that rather than a named action. Listed keys must resolve to
     something else. Exclude `g`: only `gg` is listed, and a lone `g` arms the region's pending
     marker instead of ending it. That case is checked separately below. */
  it("every other plain letter ends VISUAL (vend) rather than doing one of VISUAL_KEYS's own things", () => {
    const known = new Set(VISUAL_KEYS.flatMap((row) => row.keys.split(" / ")));
    for (const letter of "abcdefhijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".split("")) {
      if (known.has(letter)) continue;
      const ev = /[A-Z]/.test(letter) ? key(letter, { shiftKey: true }) : key(letter);
      const result = resolveKey("visual", ev, ctx);
      expect(result, `"${letter}"`).toEqual({ kind: "vend", key: letter });
    }
  });

  it("a lone g arms the region-local pending marker rather than ending the region", () => {
    expect(resolveKey("visual", key("g"), ctx)).toEqual({ kind: "pending", prefix: "g" });
    expect(resolveKey("vline", key("g"), ctx)).toEqual({ kind: "pending", prefix: "g" });
  });

  it("gg places the caret at the list's first selectable character", () => {
    expect(resolveKey("visual", key("g"), { ...ctx, pending: "g" })).toEqual({ kind: "vmove", motion: "gg" });
    expect(resolveKey("visual", key("G", { shiftKey: true }), ctx)).toEqual({ kind: "vmove", motion: "G" });
  });

  it("gv is reserved: does nothing, dropping the pending g rather than starting VISUAL", () => {
    expect(resolveKey("visual", key("v"), { ...ctx, pending: "g" })).toBeNull();
  });

  it("> quotes the selection into the draft", () => {
    expect(resolveKey("visual", key(">", { shiftKey: true }), ctx)).toEqual({ kind: "vquote" });
    expect(resolveKey("vline", key(">", { shiftKey: true }), ctx)).toEqual({ kind: "vquote" });
  });

  it("VISUAL's own key or Esc goes back to CARET (vback); the other key switches (vtoggle)", () => {
    expect(resolveKey("visual", key("v"), ctx)).toEqual({ kind: "vback" });
    expect(resolveKey("visual", key("V", { shiftKey: true }), ctx)).toEqual({ kind: "vtoggle", line: true });
    expect(resolveKey("visual", key("Escape"), ctx)).toEqual({ kind: "vback" });
    expect(resolveKey("vline", key("V", { shiftKey: true }), ctx)).toEqual({ kind: "vback" });
    expect(resolveKey("vline", key("v"), ctx)).toEqual({ kind: "vtoggle", line: false });
    expect(resolveKey("vline", key("Escape"), ctx)).toEqual({ kind: "vback" });
  });
});

describe("CARET_KEYS <-> resolveKey", () => {
  it("every key CARET_KEYS lists actually does something in CARET", () => {
    for (const { keys } of CARET_KEYS) {
      for (const token of keys.split(" / ")) {
        const ev = parseVisualToken(token);
        expect(resolveKey("caret", ev, ctx), `"${token}" (from "${keys}") in caret`).not.toBeNull();
      }
    }
  });

  it("every other plain letter ends CARET (vend), g arms the pending gg marker instead", () => {
    const known = new Set(CARET_KEYS.flatMap((row) => row.keys.split(" / ")));
    for (const letter of "abcdefhijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".split("")) {
      if (known.has(letter)) continue;
      const ev = /[A-Z]/.test(letter) ? key(letter, { shiftKey: true }) : key(letter);
      const result = resolveKey("caret", ev, ctx);
      expect(result, `"${letter}"`).toEqual({ kind: "vend", key: letter });
    }
    expect(resolveKey("caret", key("g"), ctx)).toEqual({ kind: "pending", prefix: "g" });
  });

  it("v/V from CARET always start a selection mode (vtoggle), never vback", () => {
    expect(resolveKey("caret", key("v"), ctx)).toEqual({ kind: "vtoggle", line: false });
    expect(resolveKey("caret", key("V", { shiftKey: true }), ctx)).toEqual({ kind: "vtoggle", line: true });
  });

  it("Esc ends CARET (vend), back to BROWSE -- not vback", () => {
    expect(resolveKey("caret", key("Escape"), ctx)).toEqual({ kind: "vend", key: "Escape" });
  });

  it("idle Ctrl+c in CARET is claimed (vswallow), not left to the engine's native copy", () => {
    expect(resolveKey("caret", key("c", { ctrlKey: true }), ctx)).toEqual({ kind: "vswallow" });
    expect(resolveKey("caret", key("c", { ctrlKey: true }), { ...ctx, turnRunning: true })).toEqual({ kind: "interrupt" });
  });

  it("o and y are VISUAL-only, not bound in CARET", () => {
    expect(resolveKey("caret", key("o"), ctx)).toEqual({ kind: "vend", key: "o" });
    expect(resolveKey("caret", key("y"), ctx)).toEqual({ kind: "vend", key: "y" });
  });
});

/* Keys with Ctrl, Alt, Meta, Super, Hyper or AltGraph return `vswallow` in VISUAL and
   V-LINE so browser defaults cannot run.
   Alt+j must not move the caret, and Ctrl+o must not re-render rows under the selection.
   Ctrl+c is handled separately and is covered by the freeze fixture. */
describe("a modified key in VISUAL is swallowed, VISUAL stays", () => {
  it.each([
    ["Ctrl+o", key("o", { ctrlKey: true })],
    ["Ctrl+g", key("g", { ctrlKey: true })],
    ["Ctrl+d", key("d", { ctrlKey: true })],
    ["Alt+j", key("j", { altKey: true })],
    ["Alt+y", key("y", { altKey: true })],
    ["Meta+l", key("l", { metaKey: true })],
    ["Super+a", key("a", { superKey: true })],
    ["Hyper+d", key("d", { hyperKey: true })],
    ["AltGraph+e", key("e", { altGraphKey: true })],
    ["Ctrl+Shift+V", key("V", { ctrlKey: true, shiftKey: true })],
  ] as const)("%s in VISUAL and in V-LINE is vswallow", (_name, ev) => {
    expect(resolveKey("visual", ev, ctx)).toEqual({ kind: "vswallow" });
    expect(resolveKey("vline", ev, ctx)).toEqual({ kind: "vswallow" });
  });

  it("Ctrl+c stays the exception: interrupt while a turn runs, unclaimed idle -- never vswallow", () => {
    const ctrlC = key("c", { ctrlKey: true });
    expect(resolveKey("visual", ctrlC, { ...ctx, turnRunning: true })).toEqual({ kind: "interrupt" });
    expect(resolveKey("visual", ctrlC, ctx)).toBeNull();
    // With any other modifier as well it is an ordinary modified key again.
    expect(resolveKey("visual", key("c", { ctrlKey: true, altKey: true }), ctx)).toEqual({ kind: "vswallow" });
  });
});

describe("v / V entry reads isPlainAnswerKey", () => {
  it("a plain v starts CARET, Shift+V starts V-LINE directly", () => {
    expect(resolveKey("browse", key("v"), ctx)).toEqual({ kind: "caret" });
    expect(resolveKey("browse", key("V", { shiftKey: true }), ctx)).toEqual({ kind: "visual", line: true });
  });

  /* Mode entry must reject Ctrl, Alt, Meta, Super, Hyper and AltGraph. Checking only `ctrlKey`
     lets modified `v` or Shift+V start a mode; these chords must remain unclaimed. */
  it.each([
    ["Alt", { altKey: true }],
    ["Meta", { metaKey: true }],
    ["Super", { superKey: true }],
    ["Hyper", { hyperKey: true }],
    ["AltGraph", { altGraphKey: true }],
    ["Ctrl", { ctrlKey: true }],
  ] as const)("%s+v, and Shift+V with the same modifier, start nothing", (_name, over) => {
    expect(resolveKey("browse", key("v", over), ctx)).toBeNull();
    expect(resolveKey("browse", key("V", { ...over, shiftKey: true }), ctx)).toBeNull();
  });
});

describe("the ? keymap tables", () => {
  it("are each non-empty and free of duplicate keys", () => {
    for (const table of [BROWSE_KEYS, INPUT_KEYS, CARET_KEYS, VISUAL_KEYS]) {
      expect(table.length).toBeGreaterThan(0);
      expect(new Set(table.map((row) => row.keys)).size).toBe(table.length);
    }
  });
});

/* The a/d/D cases and `App.tsx`'s bypass-confirm y share `isPlainAnswerKey`.
   Test it directly so callers can rely on its modifier and composition checks independently
   of the routing in `resolveKey`. */
describe("isPlainAnswerKey", () => {
  it("is true for a bare key, and Shift alone does not disqualify it", () => {
    expect(isPlainAnswerKey(key("a"))).toBe(true);
    expect(isPlainAnswerKey(key("D", { shiftKey: true }))).toBe(true);
  });

  it("is false with Ctrl, Alt, or Meta held", () => {
    expect(isPlainAnswerKey(key("a", { ctrlKey: true }))).toBe(false);
    expect(isPlainAnswerKey(key("a", { altKey: true }))).toBe(false);
    expect(isPlainAnswerKey(key("a", { metaKey: true }))).toBe(false);
  });

  it("is false with Super or Hyper held (the gap 325e007 had, before this reconciliation)", () => {
    expect(isPlainAnswerKey(key("a", { superKey: true }))).toBe(false);
  });

  /* `superKey` reports Super and Hyper together, which cannot catch a check narrowed to Super
     alone. Test Hyper alone directly through `isPlainAnswerKey`; routing the expected result
     through `resolveKey` could reuse the same broken `hasSuperOrHyper` and agree with the bug. */
  it("is false with Hyper held alone, not only together with Super (v1 audit fixes, finding 1)", () => {
    expect(isPlainAnswerKey(key("a", { hyperKey: true }))).toBe(false);
  });

  /* AltGr is a level-3 shift key and needs its own `getModifierState("AltGraph")` check.
     A key typed while AltGr is held must not answer a card or confirm bypass as a bare key.
     `typingGuard.ts` separately treats a bare AltGraph press as a modifier.
     These modifier-state reads have not been verified against real WebKitGTK. */
  it("is false with AltGraph held", () => {
    expect(isPlainAnswerKey(key("a", { altGraphKey: true }))).toBe(false);
  });

  it("is false while an input method is composing, by either signal", () => {
    expect(isPlainAnswerKey(key("a", { isComposing: true }))).toBe(false);
    expect(isPlainAnswerKey(key("a", { keyCode: 229 }))).toBe(false);
  });
});

/** Check that the a/d cases and Shift+D agree with `isPlainAnswerKey` for each modifier.
 *  Test Super and Hyper separately because the `superKey` helper reports both together.
 *  Ctrl+Shift+D must use key "D" to reach the Shift+D arm; lowercase "d" does not exercise it.
 *  Ctrl+d is excluded because it scrolls half a page before the answer switch is reached.
 *
 *  This matrix derives expectations from `isPlainAnswerKey`, so a bug in that predicate can
 *  make both sides agree incorrectly. Its direct tests use hardcoded false expectations,
 *  and `App.test.tsx` separately checks that no `confirm_bypass` is sent. */
type ModifierCase = [
  string,
  Partial<{
    ctrlKey: boolean;
    altKey: boolean;
    metaKey: boolean;
    superKey: boolean;
    hyperKey: boolean;
    altGraphKey: boolean;
    isComposing: boolean;
    keyCode: number;
  }>,
];
const MODIFIER_CASES: ModifierCase[] = [
  ["bare", {}],
  ["Ctrl", { ctrlKey: true }],
  ["Alt", { altKey: true }],
  ["Meta", { metaKey: true }],
  ["Super alone", { superKey: true }],
  ["Hyper alone", { hyperKey: true }],
  ["AltGraph", { altGraphKey: true }],
  ["composing", { isComposing: true }],
  ["keyCode 229", { keyCode: 229 }],
];

describe("resolveKey's a/d/Shift+D agree with isPlainAnswerKey on every modifier it reads (v1 audit fixes, finding 1)", () => {
  it.each(MODIFIER_CASES)("a: %s", (_name, mod) => {
    const ev = key("a", mod);
    const expected = isPlainAnswerKey(ev) ? { kind: "answer", decision: "allow" } : null;
    expect(resolveKey("browse", ev, ctx)).toEqual(expected);
  });

  it.each(MODIFIER_CASES.filter(([name]) => name !== "Ctrl"))("d: %s", (_name, mod) => {
    const ev = key("d", mod);
    const expected = isPlainAnswerKey(ev) ? { kind: "answer", decision: "deny" } : null;
    expect(resolveKey("browse", ev, ctx)).toEqual(expected);
  });

  it.each(MODIFIER_CASES)("Shift+D: %s", (_name, mod) => {
    const ev = key("D", { ...mod, shiftKey: true });
    const expected = isPlainAnswerKey(ev) ? { kind: "deny-reason" } : null;
    expect(resolveKey("browse", ev, ctx)).toEqual(expected);
  });
});
