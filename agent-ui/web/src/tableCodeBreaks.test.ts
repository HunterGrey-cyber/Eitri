// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { breakPointsIn, breakTableCode } from "./tableCodeBreaks";

describe("breakPointsIn", () => {
  it("breaks after a slash that has more path after it", () => {
    expect(breakPointsIn("agent/src/providers/", 0).points).toEqual([6, 10]);
  });

  it("keeps a run of slashes together and never breaks at the end", () => {
    expect(breakPointsIn("https://example.com/a", 0).points).toEqual([8, 20]);
    expect(breakPointsIn("dir/", 0).points).toEqual([]);
  });

  it("breaks after a comma only inside braces", () => {
    expect(breakPointsIn("a,b", 0).points).toEqual([]);
    expect(breakPointsIn("p/{x,y,z}/q", 0).points).toEqual([2, 5, 7, 10]);
  });

  it("hands the brace depth on to the next piece of the same code", () => {
    const first = breakPointsIn("p/{x", 0);
    expect(first.depth).toBe(1);
    expect(breakPointsIn(",y}", first.depth).points).toEqual([1]);
  });

  it("never lets a stray closing brace take the depth below zero", () => {
    expect(breakPointsIn("}},a", 0)).toEqual({ points: [], depth: 0 });
  });
});

describe("breakTableCode", () => {
  function fragment(html: string): DocumentFragment {
    const template = document.createElement("template");
    template.innerHTML = html;
    return template.content;
  }
  const html = (f: DocumentFragment) => {
    const box = document.createElement("div");
    box.append(f.cloneNode(true));
    return box.innerHTML;
  };

  it("adds a word break inside code in a table cell and nothing else", () => {
    const f = fragment("<table><tbody><tr><td><code>a/b/{c,d}</code> text/x</td></tr></tbody></table>");
    breakTableCode(f);
    expect(html(f)).toBe("<table><tbody><tr><td><code>a/<wbr>b/<wbr>{c,<wbr>d}</code> text/x</td></tr></tbody></table>");
    expect(f.textContent).toBe("a/b/{c,d} text/x");
  });

  it("leaves code outside a table, and fenced code in a cell, as it is", () => {
    const f = fragment("<p><code>a/b</code></p><table><tr><td><pre><code>a/b</code></pre></td></tr></table>");
    breakTableCode(f);
    expect(html(f)).not.toContain("<wbr>");
  });

  it("does not split an outlined escape, and keeps the depth across it", () => {
    const f = fragment('<table><tr><td><code>a/{b<span class="code-escape">⟨U+200B⟩</span>,c/d}</code></td></tr></table>');
    breakTableCode(f);
    expect(html(f)).toContain('<code>a/<wbr>{b<span class="code-escape">⟨U+200B⟩</span>,<wbr>c/<wbr>d}</code>');
  });
});
