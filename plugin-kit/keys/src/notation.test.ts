// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { notationOf, parseNotation, strokeOf } from "./notation";

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

  it("round-trips a less-than stroke followed by a named key", () => {
    const sequence = [
      { key: "<", ctrl: false, shift: true },
      { key: "Enter", ctrl: false, shift: false },
    ];
    expect(parseNotation(sequence.map(notationOf).join(""))).toEqual(sequence);
  });
});
