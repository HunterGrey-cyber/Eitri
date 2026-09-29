import { describe, expect, it } from "vitest";
import { noteKey } from "./heldSuper";
import { BROWSE_KEYS, CARET_KEYS, FIXED_PAIRS, INPUT_KEYS, VISUAL_KEYS, isPlainAnswerKey, resolveKey } from "./keymap";
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
 *  gg, [[, ]] or Ctrl+w h uses its second key with the first stored as pending. Ctrl+x carries
 *  Ctrl, G and ? carry Shift, and 1-9 uses a representative digit. Other tokens are keys on
 *  their own. Both directions of the table-to-resolveKey checks share this parser. */
function parseKeyToken(token: string): { ev: KeyLike; pending?: PendingPrefix } {
  // Ctrl+w h means h with the Ctrl+w prefix pending. Handle it before the Ctrl+ branch, which
  // would otherwise parse it as Ctrl held with a key named "w h".
  const cw = /^Ctrl\+w ([hjkl])$/.exec(token);
  if (cw) return { ev: key(cw[1]), pending: "C-w" };
  if (/^[gz\[\]].$/.test(token)) return { ev: key(token[1]), pending: token[0] as PendingPrefix };
  if (token === "1-9") return { ev: key("3") };
  // The help row uses arrow glyphs (↓ / ↑), while keyboard events report ArrowDown and ArrowUp.
  if (token === "↓") return { ev: key("ArrowDown") };
  if (token === "↑") return { ev: key("ArrowUp") };
  if (token.startsWith("Ctrl+")) return { ev: key(token.slice("Ctrl+".length), { ctrlKey: true }) };
  // `:` also carries Shift on most layouts, where it is typed as Shift+;.
  if (/^[A-Z?:]$/.test(token)) return { ev: key(token, { shiftKey: true }) };
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
    // Ctrl+f scrolls a page; Ctrl+Shift+f stays unclaimed.
    // Ctrl+b is the tmux prefix and stays unclaimed too.
    expect(resolveKey("browse", key("f", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
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

  /* Ctrl+f and PageDown/PageUp scroll a view; the arrow keys act as j/k.
     Ctrl+f must be checked before the plain-key switch because its `key` is "f", like the
     bare key that starts HINT. Ctrl+b is the tmux prefix, so PageUp scrolls back a page. */
  it("names Ctrl+f and PageDown as a page down, PageUp as a page up, and the arrows as j and k", () => {
    const cases: Array<[string, KeyLike, unknown]> = [
      ["Ctrl+f", key("f", { ctrlKey: true }), { kind: "page", delta: 1 }],
      ["PageDown", key("PageDown"), { kind: "page", delta: 1 }],
      ["PageUp", key("PageUp"), { kind: "page", delta: -1 }],
      ["ArrowDown", key("ArrowDown"), { kind: "move", delta: 1 }],
      ["ArrowUp", key("ArrowUp"), { kind: "move", delta: -1 }],
    ];
    for (const [name, ev, want] of cases) {
      expect(resolveKey("browse", ev, ctx), name).toEqual(want);
      // Reading a dead session's transcript is still reading it, as Ctrl+d/Ctrl+e are.
      expect(resolveKey("browse", ev, { sessionEnded: true }), `${name}, session ended`).toEqual(want);
      // BROWSE only: in the composer they are the text box's own (a caret line, a history walk).
      expect(resolveKey("input", ev, ctx), `${name} in INPUT`).toBeNull();
      // An input method composing owns the key (C4): its candidate list pages with these very keys.
      expect(resolveKey("browse", { ...ev, isComposing: true }, ctx), `${name} composing`).toBeNull();
      expect(resolveKey("browse", { ...ev, keyCode: 229 }, ctx), `${name} keyCode 229`).toBeNull();
    }
    // The arrows are exactly `j`/`k`: the same action, so the same tall-row scroll and the same count.
    expect(resolveKey("browse", key("ArrowDown"), ctx)).toEqual(resolveKey("browse", key("j"), ctx));
    expect(resolveKey("browse", key("ArrowUp"), ctx)).toEqual(resolveKey("browse", key("k"), ctx));
    expect(resolveKey("browse", key("ArrowDown"), { ...ctx, count: 3 })).toEqual({ kind: "move", delta: 1 });
    expect(resolveKey("browse", key("PageDown"), { ...ctx, count: 3 })).toEqual({ kind: "page", delta: 1 });
    // And the bare `f` is still HINT.
    expect(resolveKey("browse", key("f"), ctx)).toEqual({ kind: "hint" });
  });

  it("leaves the page and arrow keys unclaimed under Shift or Ctrl, and Ctrl+Shift+f, as before", () => {
    for (const over of [{ shiftKey: true }, { ctrlKey: true }, { ctrlKey: true, shiftKey: true }]) {
      for (const k of ["PageDown", "PageUp", "ArrowDown", "ArrowUp"]) {
        expect(resolveKey("browse", key(k, over), ctx), `${k} ${JSON.stringify(over)}`).toBeNull();
      }
    }
    expect(resolveKey("browse", key("f", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    // `Ctrl+b` is the prefix, so nothing here pages back with it.
    expect(resolveKey("browse", key("b", { ctrlKey: true }), ctx)).toBeNull();
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
    // After a pending `g`, the next key completes a pair or cancels the prefix.
    // A non-pair key such as `j` must not run its bare-key action.
    expect(resolveKey("browse", key("j"), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("input", key("g"), { ...ctx, pending: "g" })).toBeNull();
  });

  it("starts a global HINT on f in BROWSE, and does not in INPUT", () => {
    expect(resolveKey("browse", key("f"), ctx)).toEqual({ kind: "hint" });
    expect(resolveKey("input", key("f"), ctx)).toBeNull();
    // Ctrl+Shift+F is the shell's app accelerator, handled first by GTK. If it reaches the page,
    // it stays unclaimed, as does Shift+F. Plain Ctrl+f scrolls a page and never starts HINT.
    expect(resolveKey("browse", key("f", { ctrlKey: true, shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("f", { ctrlKey: true }), ctx)).toEqual({ kind: "page", delta: 1 });
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
    // After `[`, a non-pair key such as `j` cancels the prefix and is swallowed.
    expect(resolveKey("browse", key("j"), { ...ctx, pending: "[" }), "any other key cancels the prefix").toEqual({ kind: "cancel", why: "unbound" });
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

  /* `:` opens a command line that runs nothing, so `:ls`, `:l` and `:d` cannot answer a card.
     Match `key` with or without Shift; AZERTY types it unshifted. Ctrl, Alt or Meta exclude it.
     After a pending prefix it cancels; INPUT types it, and CARET/VISUAL end on it. */
  it("opens the : command line in BROWSE with or without Shift, and nowhere else (K02)", () => {
    expect(resolveKey("browse", key(":", { shiftKey: true }), ctx)).toEqual({ kind: "ex-line" });
    expect(resolveKey("browse", key(":"), ctx)).toEqual({ kind: "ex-line" });
    for (const over of [{ ctrlKey: true }, { altKey: true }, { metaKey: true }]) {
      expect(resolveKey("browse", key(":", { shiftKey: true, ...over }), ctx), JSON.stringify(over)).toBeNull();
      expect(resolveKey("browse", key(":", over), ctx), JSON.stringify(over)).toBeNull();
    }
    expect(resolveKey("input", key(":", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("input", key(":"), ctx)).toBeNull();
    // An input method's own key is never a command (C4), by either signal.
    expect(resolveKey("browse", key(":", { shiftKey: true, isComposing: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key(":", { shiftKey: true, keyCode: 229 }), ctx)).toBeNull();
    expect(resolveKey("browse", key(":", { shiftKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("caret", key(":", { shiftKey: true }), ctx)).toEqual({ kind: "vend", key: ":" });
    expect(resolveKey("visual", key(":", { shiftKey: true }), ctx)).toEqual({ kind: "vend", key: ":" });
  });

  it("toggles the detailed view on Ctrl+o in both modes, and scrolls a table on zh / zl", () => {
    expect(resolveKey("browse", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "detailed" });
    expect(resolveKey("input", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "detailed" });
    expect(resolveKey("browse", key("z"), ctx)).toEqual({ kind: "pending", prefix: "z" });
    expect(resolveKey("browse", key("h"), { ...ctx, pending: "z" })).toEqual({ kind: "table-scroll", delta: -1 });
    expect(resolveKey("browse", key("l"), { ...ctx, pending: "z" })).toEqual({ kind: "table-scroll", delta: 1 });
  });

  /* Each of zt/zz/zb and za/zo/zc is a row of `FIXED_PAIRS`.
     After z, the next key completes a pair or cancels; it never runs its bare-key action.
     In particular, za cannot answer a card and zo cannot start a session. */
  it("zt / zz / zb put the cursor's row at the top / middle / bottom, and za / zo / zc fold", () => {
    const z = { ...ctx, pending: "z" as const };
    expect(resolveKey("browse", key("t"), z)).toEqual({ kind: "scroll-row", where: "top" });
    expect(resolveKey("browse", key("z"), z)).toEqual({ kind: "scroll-row", where: "center" });
    expect(resolveKey("browse", key("b"), z)).toEqual({ kind: "scroll-row", where: "bottom" });
    expect(resolveKey("browse", key("a"), z)).toEqual({ kind: "toggle-expand" });
    expect(resolveKey("browse", key("o"), z)).toEqual({ kind: "fold", open: true });
    expect(resolveKey("browse", key("c"), z)).toEqual({ kind: "fold", open: false });
  });

  /* Ctrl and Shift do not complete fixed z pairs; recursive zA/zO/zC are not supported.
     Alt and Meta complete pairs as bare keys do, matching how z itself arms the prefix. */
  it("does not complete zt / zz / zb / za / zo / zc with Ctrl or Shift held: it cancels; Alt or Meta complete them", () => {
    const z = { ...ctx, pending: "z" as const };
    for (const k of ["t", "z", "b", "a", "o", "c"]) {
      for (const over of [{ ctrlKey: true }, { shiftKey: true }]) {
        // Ctrl+c remains unclaimed after a prefix while idle.
        const idleCtrlC = k === "c" && over.ctrlKey === true;
        expect(resolveKey("browse", key(k, over), z), `z ${k} with ${JSON.stringify(over)}`).toEqual(
          idleCtrlC ? null : { kind: "cancel", why: "unbound" },
        );
      }
      for (const over of [{ altKey: true }, { metaKey: true }])
        expect(resolveKey("browse", key(k, over), z), `z ${k} with ${JSON.stringify(over)}`).toEqual(resolveKey("browse", key(k), z));
    }
  });

  /* The six pairs are BROWSE-only: CARET/VISUAL never read a pending BROWSE prefix, and INPUT ignores
     it, so a `z` typed there stays exactly what it was. */
  it("leaves zt / zb / za / zo / zc alone in INPUT, where the pending prefix is never read", () => {
    for (const k of ["t", "b", "a", "o", "c"]) {
      expect(resolveKey("input", key(k), { ...ctx, pending: "z" }), k).toBeNull();
    }
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
    // A Ctrl chord cannot complete a pair. It cancels the pending `g` and is claimed so it
    // cannot reach another handler as an unrelated command.
    expect(resolveKey("browse", key("T", { ctrlKey: true, shiftKey: true }), { ...ctx, pending: "g", table })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("input", key("T", { shiftKey: true }), { ...ctx, pending: "g", table })).toBeNull();
  });
  /* Without a table binding for `[b`, `b` cancels the prefix and is swallowed, like other
     non-pair keys. */
  it("cancels [b without a table (pinned the K01 leak until X-A-11)", () => {
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[" })).toEqual({ kind: "cancel", why: "unbound" });
  });
});

/* A prefix waits for its second key without a timeout. That key completes a known pair or
   cancels the prefix; it never falls through to its own action. In particular, `g` followed
   after a pause by `d` must not deny the card under the cursor. */
describe("K01 (X-A-11): a reserved prefix's next key completes one of its pairs or cancels", () => {
  // A prefix's next key never runs its bare-key action, including page and arrow keys.
  // gp, zp and Ctrl+w p cancel; [p and ]p complete pairs.
  // zx, [x, ]x and Ctrl+w x cancel; gx completes a pair.
  const OTHER = ["a", "d", "j", "k", "i", "o", "y", "r", "n", "p", "x", "Enter", " ", "Escape", "3", "0", "/", "ArrowDown", "ArrowUp", "PageDown", "PageUp"];
  it("never falls through to the key's own meaning", () => {
    // Ctrl+w is the fifth reserved prefix and follows the same cancellation rule.
    for (const prefix of ["g", "z", "[", "]", "C-w"] as const)
      for (const k of OTHER) {
        if (FIXED_PAIRS[prefix][k] !== undefined) continue;
        expect(resolveKey("browse", key(k), { ...ctx, pending: prefix }), `${prefix} ${JSON.stringify(k)}`).toEqual({ kind: "cancel", why: "unbound" });
      }
  });
  it("cancels Shift and Alt keys too: g D is never the reason box, g G never the last row", () => {
    expect(resolveKey("browse", key("D", { shiftKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("G", { shiftKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("a", { altKey: true }), { ...ctx, pending: "]" })).toEqual({ kind: "cancel", why: "unbound" });
  });
  it("keeps every pair, gv's reservation, [] and Ctrl+c's own meaning, and waits through a bare modifier", () => {
    expect(resolveKey("browse", key("g"), { ...ctx, pending: "g" })).toEqual({ kind: "jump", to: "first" });
    expect(resolveKey("browse", key("f"), { ...ctx, pending: "g" })).toEqual({ kind: "open-path" });
    expect(resolveKey("browse", key("v"), { ...ctx, pending: "g" })).toBeNull();
    expect(resolveKey("browse", key("]"), { ...ctx, pending: "[" })).toBeNull();
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, pending: "g", turnRunning: true })).toEqual({ kind: "interrupt" });
    expect(resolveKey("browse", key("Shift", { shiftKey: true }), { ...ctx, pending: "g" })).toBeNull();
  });
  it("refuses a, d and D after a count, and leaves j a counted move", () => {
    for (const ev of [key("a"), key("d"), key("D", { shiftKey: true })])
      expect(resolveKey("browse", ev, { ...ctx, count: 3 })).toEqual({ kind: "cancel", why: "count-on-answer" });
    expect(resolveKey("browse", key("j"), { ...ctx, count: 3 })).toEqual({ kind: "move", delta: 1 });
  });
  /* Resolve pending prefixes before Ctrl+o so g then Ctrl+o cannot toggle the view.
     Every chord except Ctrl+c cancels a waiting prefix. INPUT ignores pending prefixes,
     so its Ctrl+o still toggles the detailed view. */
  it("cancels every chord but Ctrl+c: g Ctrl+o never toggles the detailed view", () => {
    // Ctrl+w cancels a pending prefix without arming another one.
    // This also makes Ctrl+w Ctrl+w and Ctrl+w Ctrl+o cancel.
    for (const prefix of ["g", "z", "[", "]", "C-w"] as const)
      for (const k of ["o", "d", "u", "e", "y", "g", "f", "w"])
        expect(resolveKey("browse", key(k, { ctrlKey: true }), { ...ctx, pending: prefix }), `${prefix} Ctrl+${k}`).toEqual({
          kind: "cancel",
          why: "unbound",
        });
    expect(resolveKey("browse", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "detailed" });
    expect(resolveKey("input", key("o", { ctrlKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "detailed" });
  });
  /* Alt and Meta are ignored on both keys of fixed pairs starting with g, z, [ or ]. This lets [[ and ]] work on
     layouts that type brackets with Option, including macOS German, French and Swiss.
     A key that completes no pair still cancels; Ctrl and Shift never complete a fixed pair. */
  it("reads Alt and Meta on a pair's second key as its first key does: [[ typed with Option still jumps", () => {
    for (const over of [{ altKey: true }, { metaKey: true }]) {
      const held = JSON.stringify(over);
      for (const prefix of ["g", "z", "[", "]"] as const) {
        expect(resolveKey("browse", key(prefix, over), ctx), `${held} ${prefix} arms`).toEqual({ kind: "pending", prefix });
        for (const [k, pair] of Object.entries(FIXED_PAIRS[prefix]))
          expect(resolveKey("browse", key(k, over), { ...ctx, pending: prefix }), `${prefix} then ${held} ${k}`).toEqual(pair);
      }
      // Reserved `gv` and mismatched bracket pairs remain unclaimed.
      expect(resolveKey("browse", key("v", over), { ...ctx, pending: "g" }), `g then ${held} v`).toBeNull();
      expect(resolveKey("browse", key("]", over), { ...ctx, pending: "[" }), `[ then ${held} ]`).toBeNull();
      expect(resolveKey("browse", key("[", over), { ...ctx, pending: "]" }), `] then ${held} [`).toBeNull();
      for (const [prefix, k] of [["]", "a"], ["g", "d"], ["[", "j"], ["z", "i"]] as const)
        expect(resolveKey("browse", key(k, over), { ...ctx, pending: prefix }), `${prefix} then ${held} ${k}`).toEqual({
          kind: "cancel",
          why: "unbound",
        });
    }
    for (const over of [{ ctrlKey: true }, { shiftKey: true }])
      expect(resolveKey("browse", key("[", over), { ...ctx, pending: "[" }), JSON.stringify(over)).toEqual({ kind: "cancel", why: "unbound" });
  });
});

/* In BROWSE, Ctrl+w h/j/k/l selects the module in that direction.
   Ctrl+w waits for its next key without a timeout; the key completes a pair or cancels,
   so Ctrl+w followed by d cannot deny a card. `resolveKey` returns a `pane` direction,
   which `App.tsx` posts to the shell as `pane_nav`. INPUT keeps the composer's word deletion,
   and CARET/VISUAL swallow the chord. */
describe("Ctrl+w h/j/k/l: the module that way", () => {
  const ctrlW = key("w", { ctrlKey: true });
  const PAIRS = [
    ["h", "left"],
    ["j", "down"],
    ["k", "up"],
    ["l", "right"],
  ] as const;

  it("arms a prefix on a plain Ctrl+w, on a live session and on one that ended", () => {
    expect(resolveKey("browse", ctrlW, ctx)).toEqual({ kind: "pending", prefix: "C-w" });
    // A dead session's transcript is still a BROWSE panel with modules around it, as for Ctrl+d.
    expect(resolveKey("browse", ctrlW, { sessionEnded: true })).toEqual({ kind: "pending", prefix: "C-w" });
    // A count typed before it is the caller's to carry to the second key (`case "pending"`).
    expect(resolveKey("browse", ctrlW, { ...ctx, count: 3 })).toEqual({ kind: "pending", prefix: "C-w" });
  });

  it("does not arm on any other Ctrl+w chord, nor on w alone", () => {
    for (const over of [
      { ctrlKey: true, shiftKey: true },
      { ctrlKey: true, altKey: true },
      { ctrlKey: true, metaKey: true },
      { ctrlKey: true, superKey: true },
      { ctrlKey: true, hyperKey: true },
      { ctrlKey: true, altGraphKey: true },
    ])
      expect(resolveKey("browse", key("w", over), ctx), JSON.stringify(over)).toBeNull();
    expect(resolveKey("browse", key("w"), ctx), "bare w is not a BROWSE key").toBeNull();
    expect(resolveKey("browse", key("W", { shiftKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("w", { altKey: true }), ctx)).toBeNull();
  });

  it("completes h, j, k and l to a pane move: left, down, up, right", () => {
    for (const [k, direction] of PAIRS) {
      expect(resolveKey("browse", key(k), { ...ctx, pending: "C-w" }), `Ctrl+w ${k}`).toEqual({ kind: "pane", direction });
      // The move does not need a live session either, and a count typed before the prefix is ignored.
      expect(resolveKey("browse", key(k), { sessionEnded: true, pending: "C-w" }), `Ctrl+w ${k}, ended`).toEqual({ kind: "pane", direction });
      expect(resolveKey("browse", key(k), { ...ctx, pending: "C-w", count: 3 }), `3 Ctrl+w ${k}`).toEqual({ kind: "pane", direction });
    }
  });

  it("knows exactly those four pairs", () => {
    expect(Object.keys(FIXED_PAIRS["C-w"]).sort()).toEqual(["h", "j", "k", "l"]);
  });

  /* After Ctrl+w, a non-pair key is swallowed so it cannot answer a card or run its own action.
     Holding Ctrl on the second key also cancels. The shell claims Ctrl+l before the page through
     `nav_key` and drops the prefix. */
  it("cancels every other key, so Ctrl+w then d is never a deny", () => {
    for (const k of ["a", "d", "i", "o", "y", "r", "n", "f", "g", "w", "Enter", " ", "Escape", "3", "/", "ArrowDown", "PageDown"])
      expect(resolveKey("browse", key(k), { ...ctx, pending: "C-w" }), `Ctrl+w ${JSON.stringify(k)}`).toEqual({ kind: "cancel", why: "unbound" });
    for (const [k, over] of [
      ["D", { shiftKey: true }],
      ["H", { shiftKey: true }],
      ["L", { shiftKey: true }],
      ["l", { ctrlKey: true }],
      ["h", { ctrlKey: true }],
      ["j", { ctrlKey: true, shiftKey: true }],
    ] as const)
      expect(resolveKey("browse", key(k, over), { ...ctx, pending: "C-w" }), `Ctrl+w ${k} ${JSON.stringify(over)}`).toEqual({ kind: "cancel", why: "unbound" });
    // A count typed after the prefix is not a pair either.
    expect(resolveKey("browse", key("3"), { ...ctx, pending: "C-w" })).toEqual({ kind: "cancel", why: "unbound" });
  });

  it("waits through a bare modifier, and keeps Ctrl+c's own meaning after it", () => {
    for (const k of ["Control", "Shift", "Alt", "Meta"])
      expect(resolveKey("browse", key(k, { ctrlKey: k === "Control" }), { ...ctx, pending: "C-w" }), k).toBeNull();
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, pending: "C-w", turnRunning: true })).toEqual({ kind: "interrupt" });
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, pending: "C-w" })).toBeNull();
  });

  it("gives the key to an input method that is composing, after the prefix too (C4)", () => {
    expect(resolveKey("browse", key("w", { ctrlKey: true, isComposing: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("w", { ctrlKey: true, keyCode: 229 }), ctx)).toBeNull();
    for (const [k] of PAIRS) {
      expect(resolveKey("browse", key(k, { isComposing: true }), { ...ctx, pending: "C-w" }), k).toBeNull();
      expect(resolveKey("browse", key(k, { keyCode: 229 }), { ...ctx, pending: "C-w" }), k).toBeNull();
    }
  });

  it("leaves INPUT and the selecting modes alone: Ctrl+w is the composer's, and CARET/VISUAL swallow it", () => {
    expect(resolveKey("input", ctrlW, ctx), "the composer deletes a word").toBeNull();
    for (const [k] of PAIRS) expect(resolveKey("input", key(k), { ...ctx, pending: "C-w" }), `INPUT never reads a prefix: ${k}`).toBeNull();
    for (const mode of ["caret", "visual", "vline"] as const)
      expect(resolveKey(mode, ctrlW, ctx), mode).toEqual({ kind: "vswallow" });
  });

  it("names the four pairs in one help row, tied to resolveKey by the round trip below", () => {
    expect(BROWSE_KEYS).toContainEqual({
      keys: "Ctrl+w h / Ctrl+w j / Ctrl+w k / Ctrl+w l",
      what: "The keys to the module that way, as Ctrl+h/j/k/l; Ctrl+w j is the module below, never the box",
    });
  });
});

/* ]p and [p move to the next or previous card waiting for an answer, wrapping at the ends.
   `resolveKey` returns only `{ kind: "card-jump", delta }`; `App.tsx` selects the card from
   the timeline and session state. The jump answers nothing. A later bare a answers the
   selected card only if it can still be answered. */
describe("]p / [p: the next / previous waiting card", () => {
  it("completes ] p to a forward jump and [ p to a backward one", () => {
    expect(resolveKey("browse", key("p"), { ...ctx, pending: "]" })).toEqual({ kind: "card-jump", delta: 1 });
    expect(resolveKey("browse", key("p"), { ...ctx, pending: "[" })).toEqual({ kind: "card-jump", delta: -1 });
  });

  it("is a motion, whatever the session or a count: App decides what waits", () => {
    for (const [prefix, delta] of [["]", 1], ["[", -1]] as const) {
      expect(resolveKey("browse", key("p"), { sessionEnded: true, pending: prefix }), `${prefix}p, ended`).toEqual({ kind: "card-jump", delta });
      expect(resolveKey("browse", key("p"), { ...ctx, pending: prefix, count: 3 }), `3${prefix}p`).toEqual({ kind: "card-jump", delta });
      expect(resolveKey("browse", key("p"), { ...ctx, pending: prefix, turnRunning: true }), `${prefix}p, running`).toEqual({ kind: "card-jump", delta });
    }
  });

  it("reads Alt and Meta on the p as the other pairs do (a bracket typed with Option), and Ctrl or Shift never", () => {
    for (const over of [{ altKey: true }, { metaKey: true }])
      expect(resolveKey("browse", key("p", over), { ...ctx, pending: "]" }), JSON.stringify(over)).toEqual({ kind: "card-jump", delta: 1 });
    expect(resolveKey("browse", key("p", { ctrlKey: true }), { ...ctx, pending: "]" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("P", { shiftKey: true }), { ...ctx, pending: "]" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("P", { shiftKey: true }), { ...ctx, pending: "[" })).toEqual({ kind: "cancel", why: "unbound" });
  });

  it("gives the p to an input method that is composing, on either bracket", () => {
    for (const pending of ["[", "]"] as const) {
      expect(resolveKey("browse", key("p", { isComposing: true }), { ...ctx, pending }), pending).toBeNull();
      expect(resolveKey("browse", key("p", { keyCode: 229 }), { ...ctx, pending }), pending).toBeNull();
    }
  });

  it("is BROWSE's: INPUT never reads a prefix, and CARET/VISUAL end at the bracket", () => {
    expect(resolveKey("input", key("p"), { ...ctx, pending: "]" })).toBeNull();
    expect(resolveKey("input", key("p"), { ...ctx, pending: "[" })).toBeNull();
    for (const mode of ["caret", "visual", "vline"] as const) {
      expect(resolveKey(mode, key("]"), ctx), `${mode} ]`).toEqual({ kind: "vend", key: "]" });
      expect(resolveKey(mode, key("["), ctx), `${mode} [`).toEqual({ kind: "vend", key: "[" });
    }
  });

  it("is the pair of two brackets only: gp, zp and Ctrl+w p cancel, and a bare p is not a BROWSE key", () => {
    for (const prefix of ["g", "z", "C-w"] as const)
      expect(resolveKey("browse", key("p"), { ...ctx, pending: prefix }), `${prefix} p`).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("p"), ctx)).toBeNull();
    expect(Object.keys(FIXED_PAIRS["["]).sort()).toEqual(["[", "p"]);
    expect(Object.keys(FIXED_PAIRS["]"]).sort()).toEqual(["]", "p"]);
    // The mismatched bracket pair stays nothing, as before.
    expect(resolveKey("browse", key("]"), { ...ctx, pending: "[" })).toBeNull();
  });

  it("leaves a table pair under the same bracket alone: [b still reaches the panel table, and [p the card jump", () => {
    expect(resolveKey("browse", key("b"), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "panel", binding: TABLE.bindings[0] });
    expect(resolveKey("browse", key("p"), { ...ctx, pending: "[", table: TABLE })).toEqual({ kind: "card-jump", delta: -1 });
    expect(resolveKey("browse", key("p"), { ...ctx, pending: "]", table: TABLE })).toEqual({ kind: "card-jump", delta: 1 });
  });

  it("names both keys in one help row, tied to resolveKey by the round trips below", () => {
    expect(BROWSE_KEYS).toContainEqual({
      keys: "]p / [p",
      what: "Next / previous card waiting for an answer (wraps; the cursor moves, nothing is answered)",
    });
  });
});

/* gx opens the web links in the cursor's row. `resolveKey` returns `{ kind: "open-link" }`;
   `App.tsx` reads the DOM and either opens a single link or waits for a choice.
   Like gf, gx completes the g prefix rather than running x as a bare key. */
describe("gx: open this row's web link", () => {
  it("completes g x to the open-link action, whatever the session or a count", () => {
    expect(resolveKey("browse", key("x"), { ...ctx, pending: "g" })).toEqual({ kind: "open-link" });
    // Like `gf`: an ended session still has links to open, and a count is ignored by App, not refused here.
    expect(resolveKey("browse", key("x"), { sessionEnded: true, pending: "g" })).toEqual({ kind: "open-link" });
    expect(resolveKey("browse", key("x"), { ...ctx, pending: "g", count: 3 })).toEqual({ kind: "open-link" });
    expect(resolveKey("browse", key("x"), { ...ctx, pending: "g", turnRunning: true })).toEqual({ kind: "open-link" });
  });

  it("reads Alt and Meta on the x as the other pairs do, and Ctrl or Shift never", () => {
    for (const over of [{ altKey: true }, { metaKey: true }])
      expect(resolveKey("browse", key("x", over), { ...ctx, pending: "g" }), JSON.stringify(over)).toEqual({ kind: "open-link" });
    expect(resolveKey("browse", key("x", { ctrlKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("X", { shiftKey: true }), { ...ctx, pending: "g" })).toEqual({ kind: "cancel", why: "unbound" });
  });

  it("gives the x to an input method that is composing", () => {
    expect(resolveKey("browse", key("x", { isComposing: true }), { ...ctx, pending: "g" })).toBeNull();
    expect(resolveKey("browse", key("x", { keyCode: 229 }), { ...ctx, pending: "g" })).toBeNull();
  });

  it("is a pair of g alone: zx, [x, ]x and Ctrl+w x cancel, and a bare x is not a BROWSE key", () => {
    for (const prefix of ["z", "[", "]", "C-w"] as const)
      expect(resolveKey("browse", key("x"), { ...ctx, pending: prefix }), `${prefix} x`).toEqual({ kind: "cancel", why: "unbound" });
    expect(resolveKey("browse", key("x"), ctx)).toBeNull();
    expect(Object.keys(FIXED_PAIRS.g).sort()).toEqual(["f", "g", "x"]);
  });

  it("is BROWSE's: INPUT never reads a prefix, and CARET/VISUAL end the region on the x", () => {
    expect(resolveKey("input", key("x"), { ...ctx, pending: "g" })).toBeNull();
    for (const mode of ["caret", "visual", "vline"] as const) {
      // A region has no `gx`: its own `g` arms its own marker (`gg`), and any key it does not bind ends it.
      expect(resolveKey(mode, key("x"), { ...ctx, pending: "g" }), `${mode} g x`).toEqual({ kind: "vend", key: "x" });
      expect(resolveKey(mode, key("x"), ctx), `${mode} x`).toEqual({ kind: "vend", key: "x" });
    }
  });

  it("names the key in a help row after gf's, and reads the HINT row as reaching links too", () => {
    const keys = BROWSE_KEYS.map((row) => row.keys);
    expect(keys.indexOf("gx")).toBe(keys.indexOf("gf") + 1);
    expect(BROWSE_KEYS).toContainEqual({
      keys: "gx",
      what: "Open this row's web link (several, or a titled one: pick by letter, each shown in full)",
    });
    expect(BROWSE_KEYS).toContainEqual({ keys: "f", what: "HINT: jump anywhere in the window (links too; Enter opens one)" });
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
        const result = resolveKey("browse", ev, localCtx);
        expect(result, `"${token}" (from "${keys}")`).not.toBeNull();
        // A key after a pending prefix that completes no pair resolves to `cancel`: claimed,
        // not `null`, but it does nothing. A help row for that key would promise an unavailable action.
        expect(result?.kind, `"${token}" (from "${keys}") only cancels the prefix`).not.toBe("cancel");
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
      // Page scrolls and arrow-key j/k motions must each appear in the help table.
      { token: "Ctrl+f", ev: key("f", { ctrlKey: true }) },
      { token: "PageDown", ev: key("PageDown") },
      { token: "PageUp", ev: key("PageUp") },
      { token: "↓", ev: key("ArrowDown") },
      { token: "↑", ev: key("ArrowUp") },
      { token: "gg", ev: key("g"), pending: "g" },
      { token: "1-9", ev: key("3") },
      { token: "[[", ev: key("["), pending: "[" },
      { token: "]]", ev: key("]"), pending: "]" },
      // Next and previous waiting cards share one help row with two tokens.
      { token: "]p", ev: key("p"), pending: "]" },
      { token: "[p", ev: key("p"), pending: "[" },
      { token: "Ctrl+c", ev: key("c", { ctrlKey: true }) },
      { token: "/", ev: key("/") },
      { token: ":", ev: key(":", { shiftKey: true }) },
      { token: "N", ev: key("N", { shiftKey: true }) },
      { token: "Ctrl+o", ev: key("o", { ctrlKey: true }) },
      { token: "zh", ev: key("h"), pending: "z" },
      { token: "zl", ev: key("l"), pending: "z" },
      // Each zt/zz/zb and za/zo/zc pair in `FIXED_PAIRS` must appear in `BROWSE_KEYS`.
      { token: "zt", ev: key("t"), pending: "z" },
      { token: "zz", ev: key("z"), pending: "z" },
      { token: "zb", ev: key("b"), pending: "z" },
      { token: "za", ev: key("a"), pending: "z" },
      { token: "zo", ev: key("o"), pending: "z" },
      { token: "zc", ev: key("c"), pending: "z" },
      { token: "gf", ev: key("f"), pending: "g" },
      // gx opens this row's web link and needs a `BROWSE_KEYS` row beside gf.
      { token: "gx", ev: key("x"), pending: "g" },
      // All four Ctrl+w pairs must appear as tokens in their shared help row.
      { token: "Ctrl+w h", ev: key("h"), pending: "C-w" },
      { token: "Ctrl+w j", ev: key("j"), pending: "C-w" },
      { token: "Ctrl+w k", ev: key("k"), pending: "C-w" },
      { token: "Ctrl+w l", ev: key("l"), pending: "C-w" },
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

/** Parses `VISUAL_KEYS` tokens into events for VISUAL/V-LINE tests. `1-9` uses a representative
 *  digit, `Esc` becomes `Escape`, and `Ctrl+x` sets Ctrl with `x`. Uppercase letters and `?`
 *  carry Shift, as in `parseKeyToken`. The `gg` branch supplies only its first key. */
function parseVisualToken(token: string): KeyLike {
  if (token === "Esc") return key("Escape");
  if (token === "1-9") return key("3");
  // For `gg`, the table tests only need the first press, which arms a pending marker.
  // Separate tests check the complete sequence alongside `G`.
  if (token === "gg") return key("g");
  if (token.startsWith("Ctrl+")) return key(token.slice("Ctrl+".length), { ctrlKey: true });
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

/* Ctrl+e/Ctrl+y scroll one line in CARET, VISUAL and V-LINE, just as in BROWSE.
   Handle them in the region resolvers before the generic modifier catch-all swallows them.
   These modes return before reaching `resolveKey`'s non-INPUT scrolling branch. */
describe("v1 trial seam review finding 3: Ctrl+e/Ctrl+y scroll the region instead of vswallow", () => {
  it.each([
    ["caret", "e", 1],
    ["caret", "y", -1],
    ["visual", "e", 1],
    ["visual", "y", -1],
    ["vline", "e", 1],
    ["vline", "y", -1],
  ] as const)("%s: Ctrl+%s resolves to scroll-line (delta=%d), not vswallow", (mode, letter, delta) => {
    expect(resolveKey(mode, key(letter, { ctrlKey: true }), ctx)).toEqual({ kind: "scroll-line", delta });
  });

  it("every other Ctrl/Alt/Meta/Super/Hyper/AltGraph chord in the region is still vswallow", () => {
    expect(resolveKey("caret", key("o", { ctrlKey: true }), ctx)).toEqual({ kind: "vswallow" });
    expect(resolveKey("visual", key("d", { ctrlKey: true }), ctx)).toEqual({ kind: "vswallow" });
    expect(resolveKey("vline", key("e", { ctrlKey: true, shiftKey: true }), ctx)).toEqual({ kind: "vswallow" });
  });

  it("idle Ctrl+c in CARET is still claimed (vswallow), not mistaken for the new Ctrl+e/Ctrl+y rule", () => {
    expect(resolveKey("caret", key("c", { ctrlKey: true }), ctx)).toEqual({ kind: "vswallow" });
  });

  it("CARET_KEYS and VISUAL_KEYS both list Ctrl+e / Ctrl+y now, and the ? row says they are exceptions", () => {
    const caretRow = CARET_KEYS.find((row) => row.keys === "Ctrl+e / Ctrl+y");
    const visualRow = VISUAL_KEYS.find((row) => row.keys === "Ctrl+e / Ctrl+y");
    expect(caretRow, "CARET_KEYS should list Ctrl+e / Ctrl+y").toBeDefined();
    expect(visualRow, "VISUAL_KEYS should list Ctrl+e / Ctrl+y").toBeDefined();
    const caretHelp = CARET_KEYS.find((row) => row.keys === "?")!.what;
    const visualHelp = VISUAL_KEYS.find((row) => row.keys === "?")!.what;
    expect(caretHelp).toContain("Ctrl+e");
    expect(visualHelp).toContain("Ctrl+e");
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
