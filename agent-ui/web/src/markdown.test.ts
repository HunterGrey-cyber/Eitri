// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { renderMarkdown } from "./markdown";

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
});
