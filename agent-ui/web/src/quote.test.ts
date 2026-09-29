import { describe, expect, it } from "vitest";
import { appendQuote, formatQuote } from "./quote";

describe("formatQuote (D10, spec §3)", () => {
  it("one line becomes > line, followed by a blank line", () => {
    expect(formatQuote("The quick")).toBe("> The quick\n\n");
  });

  it("several lines each become > line", () => {
    expect(formatQuote("a\nb\nc")).toBe("> a\n> b\n> c\n\n");
  });

  it("an interior blank line becomes a bare > (the spec's own worked example)", () => {
    expect(formatQuote("Hello\n\nworld")).toBe("> Hello\n>\n> world\n\n");
  });

  it("a whitespace-only line also becomes a bare >", () => {
    expect(formatQuote("a\n   \nb")).toBe("> a\n>\n> b\n\n");
  });

  it("leading and trailing newlines are dropped before quoting", () => {
    expect(formatQuote("\n\nhello\n\n\n")).toBe("> hello\n\n");
  });

  it("\\r\\n and bare \\r are both normalised to \\n", () => {
    expect(formatQuote("a\r\nb")).toBe("> a\n> b\n\n");
    expect(formatQuote("a\rb")).toBe("> a\n> b\n\n");
  });

  it("tabs are kept as they are, not collapsed", () => {
    expect(formatQuote("a\tb")).toBe("> a\tb\n\n");
  });

  it("CJK text is kept whole", () => {
    expect(formatQuote("你好世界")).toBe("> 你好世界\n\n");
  });

  it("nothing left after trimming (all newlines, or empty) returns null -- the caller's own 'nothing to quote'", () => {
    expect(formatQuote("")).toBeNull();
    expect(formatQuote("\n\n\n")).toBeNull();
  });
});

describe("appendQuote (D10, spec §3)", () => {
  const quote = formatQuote("The quick")!; // "> The quick\n\n"

  it("an empty draft takes the quote as is", () => {
    expect(appendQuote("", quote)).toBe(quote);
  });

  it("a draft already ending in two newlines gets nothing extra between", () => {
    expect(appendQuote("hello\n\n", quote)).toBe(`hello\n\n${quote}`);
  });

  it("a draft ending in a single newline gets exactly one more", () => {
    expect(appendQuote("hello\n", quote)).toBe(`hello\n\n${quote}`);
  });

  it("a draft ending in no newline at all gets two", () => {
    expect(appendQuote("hello", quote)).toBe(`hello\n\n${quote}`);
  });

  it("two quotes in order stack, each its own blockquote", () => {
    const second = formatQuote("second thing")!;
    const once = appendQuote("", quote);
    const twice = appendQuote(once, second);
    expect(twice).toBe(`> The quick\n\n> second thing\n\n`);
  });
});
