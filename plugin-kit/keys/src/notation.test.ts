// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { canonicalOf, notationOf, parseNotation, strokeOf } from "./notation";

import { mergeRows } from "./install";
import { GENERIC_ROWS } from "./rows";

// The list the host's tests read too, so the host's canonical spelling and this parser cannot drift apart.
const list = resolve(dirname(fileURLToPath(import.meta.url)), "../../../core/src/plugin/keys_notation.json");
const shared = JSON.parse(readFileSync(list, "utf8")) as {
  keys: { notation: string; canonical: string; strokes: { key: string; ctrl: boolean; shift: boolean }[] }[];
  not_keys: string[];
};

describe("key notation", () => {
  it.each([
    ["j", [{ key: "j", ctrl: false, shift: false }]],
    ["gg", [{ key: "g", ctrl: false, shift: false }, { key: "g", ctrl: false, shift: false }]],
    ["]q", [{ key: "]", ctrl: false, shift: false }, { key: "q", ctrl: false, shift: false }]],
    ["<C-d>", [{ key: "d", ctrl: true, shift: false }]],
    ["<C-S-x>", [{ key: "x", ctrl: true, shift: true }]],
    ["<Esc>", [{ key: "Escape", ctrl: false, shift: false }]],
    ["<CR>", [{ key: "Enter", ctrl: false, shift: false }]],
    ["<BS>", [{ key: "Backspace", ctrl: false, shift: false }]],
    ["?", [{ key: "?", ctrl: false, shift: true }]],
    ["1", [{ key: "1", ctrl: false, shift: false }]],
    ["<Left>", [{ key: "ArrowLeft", ctrl: false, shift: false }]],
    ["<Right>", [{ key: "ArrowRight", ctrl: false, shift: false }]],
    ["G", [{ key: "G", ctrl: false, shift: true }]],
    ["<", [{ key: "<", ctrl: false, shift: true }]],
    ["<<", [{ key: "<", ctrl: false, shift: true }, { key: "<", ctrl: false, shift: true }]],
    ["<lt>", [{ key: "<", ctrl: false, shift: true }]],
    ["<lt><CR>", [{ key: "<", ctrl: false, shift: true }, { key: "Enter", ctrl: false, shift: false }]],
  ])("parses %s and matches keyboard events", (notation, expected) => {
    expect(parseNotation(notation as string)).toEqual(expected);
    for (const stroke of expected as { key: string; ctrl: boolean; shift: boolean }[]) {
      expect(strokeOf(new KeyboardEvent("keydown", {
        key: stroke.key, ctrlKey: stroke.ctrl, shiftKey: stroke.shift,
      }))).toEqual(stroke);
    }
  });

  it.each(["<Foo>", "<constructor>", "<toString>", "", "abcdefghijk", "<C->", "<C-C-d>", "\n"])("rejects %j", (text) => {
    expect(parseNotation(text)).toBeNull();
  });

  it("normalizes shifted control letters", () => {
    expect(strokeOf(new KeyboardEvent("keydown", { key: "X", ctrlKey: true, shiftKey: true })))
      .toEqual({ key: "x", ctrl: true, shift: true });
  });

  it.each(shared.keys.map((entry) => [entry.canonical, entry.strokes] as const))(
    "parses the host's canonical %j into the shared list's strokes", (canonical, strokes) => {
      expect(parseNotation(canonical)).toEqual(strokes);
    },
  );

  it.each(shared.not_keys)("refuses %j from the shared list", (text) => {
    expect(parseNotation(text)).toBeNull();
  });

  it.each(shared.keys)("canonicalizes $notation and takes its canonical generic key", ({ notation, canonical, strokes }) => {
    expect(canonicalOf(notation)).toBe(canonical);
    expect(parseNotation(notation)).toEqual(strokes);
    const generic = { action: "test.generic", keys: [canonical], context: "test", desc: "generic" };
    const page = { action: "test.page", keys: [notation], context: "test", desc: "page" };
    GENERIC_ROWS.push(generic);
    try {
      const merged = mergeRows([page]);
      expect(merged.some((row) => row.action === generic.action)).toBe(false);
      expect(merged).toContainEqual(page);
    } finally {
      GENERIC_ROWS.pop();
    }
  });

  it.each(shared.not_keys)("does not canonicalize invalid %j", (notation) => {
    expect(canonicalOf(notation)).toBeNull();
  });

  it.each([
    ["<esc>", "mode.escape"], ["<s-g>", "scroll.bottom"],
    ["<s-h>", "history.back"], ["<s-l>", "history.forward"],
  ])("%s removes the built-in %s", (key, action) => {
    const merged = mergeRows([{ action: "page.custom", keys: [key], context: "page", desc: "custom" }]);
    expect(merged.some((row) => row.action === action)).toBe(false);
  });

  it.each(["<s-tab>", "<S-1>", "<S-;>", "<c-s-x>", "<S-lt>"])("round-trips canonical %s", (notation) => {
    const canonical = canonicalOf(notation);
    expect(canonical).not.toBeNull();
    expect(parseNotation(canonical!)).toEqual(parseNotation(notation));
  });

  it("round-trips a less-than stroke followed by a named key", () => {
    const sequence = [
      { key: "<", ctrl: false, shift: true },
      { key: "Enter", ctrl: false, shift: false },
    ];
    expect(parseNotation(sequence.map(notationOf).join(""))).toEqual(sequence);
  });
});
