import { describe, expect, it } from "vitest";
import { resolveKey } from "./keymap";

const key = (k: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean; isComposing: boolean }> = {}) =>
  ({ key: k, ctrlKey: false, shiftKey: false, isComposing: false, ...over });
const ctx = { sessionEnded: false };

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
  });
});
