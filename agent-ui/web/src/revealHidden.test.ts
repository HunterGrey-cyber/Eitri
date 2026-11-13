import { describe, expect, it } from "vitest";
import { countEscapes, revealHidden, revealPath } from "./revealHidden";

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


describe("revealPath", () => {
  it("keeps a path on one line and counts newline, tab and carriage return escapes", () => {
    expect(revealPath("a.md: no earlier copy to compare with\nb.md\t\r")).toEqual([
      { text: "a.md: no earlier copy to compare with" }, { escape: "⟨U+000A⟩" },
      { text: "b.md" }, { escape: "⟨U+0009⟩" }, { escape: "⟨U+000D⟩" },
    ]);
    expect(countEscapes(revealPath("a\nb\tc\r"))).toBe(3);
    expect(revealHidden("a\nb\tc")).toEqual([{ text: "a\nb\tc" }]);
  });

  it("keeps all other hidden-character escapes, literal markup and ordinary path text", () => {
    for (const text of ["", "a/b c.md", "<b>file</b>", "אב.md", "a\u200bb\u202ec\u{E0041}", "\uD800"]) {
      expect(revealPath(text)).toEqual(revealHidden(text));
    }
  });
});

describe("default-ignorable code points", () => {
  it("escapes the unassigned ones too, which a renderer draws as nothing", () => {
    const text = "a\u2065b\uFFF0c\u{E0000}d\u{E0002}e\u{E0080}f\u{E01F0}g\u{E0FFF}h";
    expect(shown(text)).toBe("a⟨U+2065⟩b⟨U+FFF0⟩c⟨U+E0000⟩d⟨U+E0002⟩e⟨U+E0080⟩f⟨U+E01F0⟩g⟨U+E0FFF⟩h");
  });
});
