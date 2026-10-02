import { describe, expect, it } from "vitest";
import { countEscapes, revealHidden } from "./revealHidden";

/** What the pieces read as on screen, escapes and all. */
function shown(text: string): string {
  return revealHidden(text)
    .map((piece) => ("text" in piece ? piece.text : piece.escape))
    .join("");
}

describe("revealHidden", () => {
  it("shows a right-to-left override as a visible escape", () => {
    expect(shown("ls \u202Ex")).toBe("ls ⟨U+202E⟩x");
    expect(revealHidden("ls \u202Ex")).toEqual([{ text: "ls " }, { escape: "⟨U+202E⟩" }, { text: "x" }]);
  });

  it("escapes isolates, zero-width characters, odd spaces, tags and control characters", () => {
    for (const [ch, escape] of [
      ["\u2066", "⟨U+2066⟩"],
      ["\u2067", "⟨U+2067⟩"],
      ["\u2068", "⟨U+2068⟩"],
      ["\u2069", "⟨U+2069⟩"],
      ["\u200B", "⟨U+200B⟩"],
      ["\uFEFF", "⟨U+FEFF⟩"],
      ["\u00A0", "⟨U+00A0⟩"],
      ["\u{E0041}", "⟨U+E0041⟩"],
      ["\r", "⟨U+000D⟩"],
      ["\x1b", "⟨U+001B⟩"],
      ["\u0085", "⟨U+0085⟩"],
      ["\u2028", "⟨U+2028⟩"],
      ["\uFE0F", "⟨U+FE0F⟩"],
      ["\u3164", "⟨U+3164⟩"],
    ]) {
      expect(revealHidden(`a${ch}b`), JSON.stringify(ch)).toEqual([{ text: "a" }, { escape }, { text: "b" }]);
    }
  });

  it("leaves newlines, tabs, plain spaces and right-to-left letters alone, as one piece", () => {
    for (const text of ["a\nb\tc d", "rm -rf אב"]) {
      expect(revealHidden(text)).toEqual([{ text }]);
    }
    expect(revealHidden("")).toEqual([]);
  });

  it("escapes a lone surrogate", () => {
    expect(revealHidden("\uD800")).toEqual([{ escape: "⟨U+D800⟩" }]);
  });

  it("counts the escapes", () => {
    expect(countEscapes(revealHidden("a\u202Eb\u200Bc"))).toBe(2);
    expect(countEscapes(revealHidden("abc"))).toBe(0);
  });
});
