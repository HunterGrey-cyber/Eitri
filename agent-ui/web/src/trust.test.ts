import { describe, expect, it } from "vitest";
import { groupItemsByFile, isHex64, parseExCommand, resolveTrustKey, scrollTarget, trustFooter } from "./trust";
import type { TrustItem, TrustKeyEvent } from "./trust";

const GUARD_OPEN = { now: 1000, openedAt: 0, lastKeyAt: -Infinity, pendingG: false };

function key(k: string, init: Partial<TrustKeyEvent> = {}): TrustKeyEvent {
  return { key: k, ctrlKey: false, shiftKey: false, isComposing: false, repeat: false, ...init };
}

const item = (file: string, label: string): TrustItem => ({ what: "hook", file, label, value: "x", outside: false });

describe("resolveTrustKey", () => {
  it("answers y and n past both halves of the wait", () => {
    expect(resolveTrustKey(key("y"), GUARD_OPEN)).toEqual({ kind: "answer", trust: true });
    expect(resolveTrustKey(key("Y", { shiftKey: true }), GUARD_OPEN)).toEqual({ kind: "answer", trust: true });
    expect(resolveTrustKey(key("n"), GUARD_OPEN)).toEqual({ kind: "answer", trust: false });
  });

  it("takes no y at the question for a configuration too large to show, only n and Escape", () => {
    const only = { ...GUARD_OPEN, onlyN: true };
    expect(resolveTrustKey(key("y"), only)).toEqual({ kind: "swallow" });
    expect(resolveTrustKey(key("Y", { shiftKey: true }), only)).toEqual({ kind: "swallow" });
    expect(resolveTrustKey(key("n"), only)).toEqual({ kind: "answer", trust: false });
    expect(resolveTrustKey(key("Escape"), only)).toEqual({ kind: "cancel" });
    // Not a wait either: a y inside the wait is still no answer.
    expect(resolveTrustKey(key("y"), { ...only, now: 100 })).toEqual({ kind: "swallow" });
  });

  it("words the footer of that question without a y", () => {
    expect(trustFooter("window", false, "too_large")).toBe("n starts without it · Esc puts this off · y is not taken here");
  });

  it("refuses a y or n typed with Ctrl, Alt, Meta, Super or an input method composing", () => {
    for (const k of ["y", "n"]) {
      expect(resolveTrustKey(key(k, { ctrlKey: true }), GUARD_OPEN).kind).not.toBe("answer");
      expect(resolveTrustKey(key(k, { altKey: true }), GUARD_OPEN).kind).not.toBe("answer");
      expect(resolveTrustKey(key(k, { metaKey: true }), GUARD_OPEN).kind).not.toBe("answer");
      expect(resolveTrustKey(key(k, { isComposing: true }), GUARD_OPEN).kind).not.toBe("answer");
      expect(resolveTrustKey(key(k, { keyCode: 229 }), GUARD_OPEN).kind).not.toBe("answer");
      expect(
        resolveTrustKey(key(k, { getModifierState: (m: string) => m === "Super" }), GUARD_OPEN).kind,
      ).not.toBe("answer");
    }
  });

  it("refuses a y or n inside the wait since the prompt opened or since the last key", () => {
    expect(resolveTrustKey(key("y"), { ...GUARD_OPEN, now: 100 })).toEqual({ kind: "wait" });
    expect(resolveTrustKey(key("n"), { ...GUARD_OPEN, now: 100 })).toEqual({ kind: "wait" });
    expect(resolveTrustKey(key("y"), { ...GUARD_OPEN, lastKeyAt: 900 })).toEqual({ kind: "wait" });
    expect(resolveTrustKey(key("n"), { ...GUARD_OPEN, lastKeyAt: 900 })).toEqual({ kind: "wait" });
  });

  it("never counts a held key's repeat", () => {
    expect(resolveTrustKey(key("y", { repeat: true }), GUARD_OPEN)).toEqual({ kind: "wait" });
    expect(resolveTrustKey(key("n", { repeat: true }), GUARD_OPEN)).toEqual({ kind: "wait" });
  });

  it("puts the start off on Escape, with no wait", () => {
    expect(resolveTrustKey(key("Escape"), { ...GUARD_OPEN, now: 1 })).toEqual({ kind: "cancel" });
  });

  it("scrolls on j, k, Ctrl+d, Ctrl+u, gg and G, and swallows every other key", () => {
    expect(resolveTrustKey(key("j"), GUARD_OPEN)).toEqual({ kind: "scroll", by: "line-down" });
    expect(resolveTrustKey(key("k"), GUARD_OPEN)).toEqual({ kind: "scroll", by: "line-up" });
    expect(resolveTrustKey(key("d", { ctrlKey: true }), GUARD_OPEN)).toEqual({ kind: "scroll", by: "half-down" });
    expect(resolveTrustKey(key("u", { ctrlKey: true }), GUARD_OPEN)).toEqual({ kind: "scroll", by: "half-up" });
    expect(resolveTrustKey(key("G", { shiftKey: true }), GUARD_OPEN)).toEqual({ kind: "scroll", by: "bottom" });
    expect(resolveTrustKey(key("g"), GUARD_OPEN)).toEqual({ kind: "pending_g" });
    expect(resolveTrustKey(key("g"), { ...GUARD_OPEN, pendingG: true })).toEqual({ kind: "scroll", by: "top" });
    for (const k of ["Enter", "q", "a", " ", "ArrowDown", "Tab", "i"]) {
      expect(resolveTrustKey(key(k), GUARD_OPEN), k).toEqual({ kind: "swallow" });
    }
    expect(resolveTrustKey(key("j", { altKey: true }), GUARD_OPEN)).toEqual({ kind: "swallow" });
    expect(resolveTrustKey(key("x", { ctrlKey: true }), GUARD_OPEN)).toEqual({ kind: "swallow" });
  });
});

describe("scrollTarget", () => {
  const box = { scrollTop: 100, scrollHeight: 1000, clientHeight: 200 };
  it("steps, pages and jumps, clamped to the box", () => {
    expect(scrollTarget("line-down", box, 40)).toBe(140);
    expect(scrollTarget("line-up", box, 40)).toBe(60);
    expect(scrollTarget("half-down", box, 40)).toBe(200);
    expect(scrollTarget("half-up", box, 40)).toBe(0);
    expect(scrollTarget("top", box, 40)).toBe(0);
    expect(scrollTarget("bottom", box, 40)).toBe(800);
    expect(scrollTarget("line-down", { ...box, scrollTop: 790 }, 40)).toBe(800);
    expect(scrollTarget("bottom", { scrollTop: 0, scrollHeight: 100, clientHeight: 200 }, 40)).toBe(0);
  });
});

describe("groupItemsByFile", () => {
  it("groups by file in the order files first appear, keeping each file's own order", () => {
    const groups = groupItemsByFile([item("a", "1"), item("b", "2"), item("a", "3")]);
    expect(groups.map((g) => g.file)).toEqual(["a", "b"]);
    expect(groups[0].items.map((i) => i.label)).toEqual(["1", "3"]);
  });
  it("is empty for no items", () => {
    expect(groupItemsByFile([])).toEqual([]);
  });
});

describe("parseExCommand", () => {
  it("takes the two trust words exactly, ignoring only surrounding space", () => {
    expect(parseExCommand("trust")).toEqual({ command: "trust" });
    expect(parseExCommand(" untrust ")).toEqual({ command: "untrust" });
    for (const line of ["", "trust x", "trusted", "Trust", "trust untrust", "tru", "ls", "untrust codex"]) {
      expect(parseExCommand(line), line).toBeNull();
    }
  });

  it("takes :tabnew with at most one word, and leaves judging the word to the caller", () => {
    expect(parseExCommand("tabnew")).toEqual({ command: "tabnew", agent: null });
    expect(parseExCommand(" tabnew  codex ")).toEqual({ command: "tabnew", agent: "codex" });
    expect(parseExCommand("tabnew gemini")).toEqual({ command: "tabnew", agent: "gemini" });
    for (const line of ["tabnew codex claude", "tabnewcodex", "Tabnew", "tabn codex"]) {
      expect(parseExCommand(line), line).toBeNull();
    }
  });
});

describe("the wire checks", () => {
  it("accepts 64 lowercase hex digits only", () => {
    expect(isHex64("a".repeat(64))).toBe(true);
    expect(isHex64("a".repeat(63))).toBe(false);
    expect(isHex64("A".repeat(64))).toBe(false);
    expect(isHex64("g".repeat(64))).toBe(false);
    expect(isHex64(7)).toBe(false);
    expect(isHex64(undefined)).toBe(false);
  });
  it("adds the one-start-only line for a session prompt", () => {
    expect(trustFooter("yes")).toBe("y trusts and loads it · n starts without it · Esc puts this off");
    expect(trustFooter("window")).toBe(trustFooter("yes"));
    expect(trustFooter("session")).toContain("y trusts this start only");
    expect(trustFooter("yes", true)).toBe("y trusts and starts it · n does not start it · Esc puts this off");
    expect(trustFooter("session", true)).toContain("n does not start it");
  });
});
