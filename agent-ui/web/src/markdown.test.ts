// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { MARKDOWN_CACHE_LIMIT, markdownParseCount, renderMarkdown } from "./markdown";

describe("renderMarkdown", () => {
  it("highlights a fenced block in a language it knows", () => {
    const html = renderMarkdown("```rust\nfn main() {}\n```");
    expect(html).toContain("hljs-keyword");
    expect(html).toContain("main");
  });

  it("leaves an unknown language as plain monospace rather than guessing", () => {
    const html = renderMarkdown("```brainfuck\n+++++\n```");
    expect(html).not.toContain("hljs-keyword");
    expect(html).toContain("+++++");
  });

  it("still sanitizes, and sanitizes the highlighted HTML", () => {
    const html = renderMarkdown("<img src=x onerror=alert(1)>\n\n```rust\nfn main() {}\n```");
    expect(html).not.toContain("onerror");
    expect(html).toContain("hljs-keyword");
  });

  it("sanitizes AFTER markdown parsing, not before -- a URL only dangerous once rendered as an anchor", () => {
    // "[x](javascript:alert(1))" is inert as SOURCE TEXT -- there is no HTML in it yet for
    // DOMPurify to act on. marked turns it into `<a href="javascript:alert(1)">x</a>`, which is
    // dangerous. `DOMPurify.sanitize(marked.parse(text))` (the real order) strips the href
    // entirely; `marked.parse(DOMPurify.sanitize(text))` (parse-then-sanitize swapped, i.e.
    // sanitize-then-parse) sanitizes the harmless source string and hands its output straight to
    // marked with no sanitize afterwards, so the dangerous href survives untouched.
    //
    // Confirmed by mutating `renderMarkdown` to that swapped order: this assertion fails
    // (`href="javascript:alert(1)"` comes through) while the three tests above still pass -- this
    // is the one test in this file that pins the ORDER, not just that sanitizing happens at all.
    const html = renderMarkdown("[x](javascript:alert(1))");
    expect(html).not.toContain("javascript:");
  });

  it("wraps a table in its own sideways scroller (T1)", () => {
    const html = renderMarkdown("| a | b |\n|---|---|\n| 1 | 2 |");
    expect(html).toMatch(/<div class="table-scroll"><table>[\s\S]*<\/table><\/div>/);
  });

  it("parses a text once and serves the rest from a bounded cache (V3)", () => {
    const before = markdownParseCount();
    const a = renderMarkdown("once **only**");
    expect(renderMarkdown("once **only**")).toBe(a);
    expect(markdownParseCount() - before).toBe(1);
    for (let i = 0; i < MARKDOWN_CACHE_LIMIT + 5; i++) renderMarkdown(`filler ${i}`);
    const again = markdownParseCount();
    renderMarkdown("once **only**");
    expect(markdownParseCount() - again, "the oldest entry was evicted").toBe(1);
  });
});
