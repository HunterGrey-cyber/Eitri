import { describe, expect, it } from "vitest";
import {
  caretOnFirstLine,
  caretOnLastLine,
  COMPOSER_CHORDS,
  enterNewline,
  growHeight,
  isImeKey,
  lineEdge,
  mergeTaken,
  readlineEdit,
} from "./composerKeys";
import { INPUT_KEYS } from "./keymap";

describe("composer keys", () => {
  it("treats an IME's key as the IME's, however WebKit reports it", () => {
    expect(isImeKey({ isComposing: true, keyCode: 13 })).toBe(true);
    expect(isImeKey({ isComposing: false, keyCode: 229 })).toBe(true);
    expect(isImeKey({ isComposing: false, keyCode: 13 })).toBe(false);
  });

  it("deletes a word back with Ctrl+w and to the line's start with Ctrl+u, as readline does", () => {
    expect(readlineEdit("git log --oneline", 17, 17, "w")).toEqual({ value: "git log ", caret: 8 });
    expect(readlineEdit("git log   ", 10, 10, "w")).toEqual({ value: "git ", caret: 4 });
    expect(readlineEdit("line one\nline two", 17, 17, "u")).toEqual({ value: "line one\n", caret: 9 });
    expect(readlineEdit("abc", 0, 0, "w")).toBeNull();
    expect(readlineEdit("abc def", 0, 7, "w"), "a selection is deleted whole").toEqual({ value: "", caret: 0 });
  });

  it("lineEdge: the start and the end of the line the caret is on, as readline's Ctrl+a / Ctrl+e", () => {
    const text = "first line\nsecond\nthird";
    expect(lineEdge(text, 14, "start")).toBe(11);
    expect(lineEdge(text, 14, "end")).toBe(17);
    expect(lineEdge(text, 3, "start")).toBe(0);
    expect(lineEdge(text, 3, "end")).toBe(10);
    expect(lineEdge(text, 20, "end"), "the last line ends at the text's end").toBe(text.length);
    expect(lineEdge(text, 11, "start"), "already at a line's start").toBe(11);
    expect(lineEdge(text, 10, "end"), "already at a line's end").toBe(10);
    expect(lineEdge("", 0, "end")).toBe(0);
  });

  it("enterNewline: Alt+Enter and a backslash before the caret are newlines; every other Enter is not", () => {
    const none = { altKey: false, ctrlKey: false, metaKey: false };
    expect(enterNewline("abcd", 2, 2, { ...none, altKey: true })).toEqual({ value: "ab\ncd", caret: 3 });
    expect(enterNewline("abcd", 1, 3, { ...none, altKey: true })).toEqual({ value: "a\nd", caret: 2 });
    expect(enterNewline("line\\", 5, 5, none)).toEqual({ value: "line\n", caret: 5 });
    expect(enterNewline("ab\\cd", 3, 3, none)).toEqual({ value: "ab\ncd", caret: 3 });
    expect(enterNewline("hello", 5, 5, none)).toBeNull();
    expect(enterNewline("a\\b", 3, 3, none), "a backslash further back").toBeNull();
    expect(enterNewline("a\\bc", 2, 3, none), "a selection").toBeNull();
    expect(enterNewline("x\\", 2, 2, { ...none, ctrlKey: true })).toBeNull();
    expect(enterNewline("x\\", 2, 2, { ...none, metaKey: true })).toBeNull();
    expect(enterNewline("x", 1, 1, { ...none, altKey: true, ctrlKey: true })).toBeNull();
    expect(enterNewline("\\", 0, 0, none)).toBeNull();
  });

  it("knows whether the caret is on the box's first or last line", () => {
    expect(caretOnFirstLine("one\ntwo", 2)).toBe(true);
    expect(caretOnFirstLine("one\ntwo", 5)).toBe(false);
    expect(caretOnLastLine("one\ntwo", 5)).toBe(true);
    expect(caretOnLastLine("one\ntwo", 2)).toBe(false);
  });

  it("grows the box to its content, at most 40% of the panel", () => {
    expect(growHeight(60, 1000)).toBe(60);
    expect(growHeight(900, 1000)).toBe(400);
    expect(growHeight(20, 1000), "one line stays one line").toBe(20);
  });

  it("puts the queue back ahead of what is in the box, blank-line separated", () => {
    expect(mergeTaken(["a", "b"], "")).toBe("a\n\nb");
    expect(mergeTaken(["a"], "draft")).toBe("a\n\ndraft");
    expect(mergeTaken([], "draft")).toBe("draft");
  });

  it("lists exactly the keys the composer implements, both ways", () => {
    expect(new Set(INPUT_KEYS.map((row) => row.keys))).toEqual(new Set(COMPOSER_CHORDS));
  });
});
