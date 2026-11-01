// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { markdownParseCount, renderMarkdown, streamMarkdown, type StreamedMarkdown } from "./markdown";

/* A reply being written is drawn in pieces (`streamMarkdown`); what it draws must be, to the character and at every
   point the batches can fall on, what one rendering of the same text gives. */

const AWKWARD: Record<string, string> = {
  "paragraphs": "one\n\ntwo\n\nthree\n",
  "leading blank lines": "\n\n\nfirst\n\nsecond",
  "only spaces": "   \n\n   ",
  "trailing spaces on a blank line": "a\n   \nb\n \n \nc",
  "a closed fence": "before\n\n```rust\nfn main() {}\n```\n\nafter\n",
  "fences with no blank lines": "```\na\n```\n```\nb\n```\ntext after\n```\nc\n```\n",
  "an unclosed fence": "before\n\n```rust\nfn main() {\n\n    let a = 1;\n\n",
  "a tilde fence": "~~~\n```\ninner\n```\n~~~\n\nafter\n",
  "a longer fence holding a fence": "````md\n```rust\nx\n```\n\nblank above\n````\n\nafter\n",
  "tabs in a fence": "```go\nfunc f() {\n\tif x {\n\t\ty()\n\t}\n}\n```\n\nafter\n\n```make\nall:\n\tcc x.c\n```\n",
  "tabs outside a fence": "\tindented code\n\n\tmore of it\n\npara\n\n- item\n\n\tinside the item\n",
  "a fence right after a heading": "# Title\n```\ncode\n```\n## Next\n\n```\nmore\n```\n\ndone",
  "headings": "# one\n\n## two\ntext\n\n### three ###\n\nsetext\n===\n\nsetext two\n---\n\nbody",
  "a setext underline arriving late": "title\n\nanother title\n=====\n\nnext\n",
  "a rule or a setext heading": "para\n---\n\n---\n\n***\n\nend",
  "an underline reaching back over a rule": "head\n***\nb\n===\n\nafter\n",
  "an underline reaching back over a fence": "```\ncode\n```\nb\n===\n\nafter\n",
  "an underline reaching back over a heading": "# one\ntwo\n---\n\nafter\n",
  "an indented line folded into a paragraph": "b\n    code\n|---|\n\n".repeat(3) + "first\n\n<div><a href=https://example.com>\n\nlink\n\n</a></div>\n\nafter\n",
  "an indented line folded into a paragraph, then blocks": "para\n    still para\n\nnext\n\n- item\n\nend\n",
  "a tight list": "- a\n- b\n- c\n\nafter\n",
  "a loose list": "- a\n\n- b\n\n- c\n\nafter\n",
  "an ordered list going on after a blank line": "1. a\n\n2. b\n\n3. c\n\nnot a list\n",
  "a number that becomes an item": "1. first\n\n2",
  "an item that goes on": "- a\n\n  still a\n\n  ```\n  code in the item\n  ```\n\n- b\n\ndone\n",
  "nested lists": "- a\n  - b\n    - c\n\n  back in a\n- d\n\n1. x\n   1. y\n",
  "a list changing its marker": "- a\n\n* b\n\n+ c\n\n1. d\n\n1) e\n",
  "a list right after a paragraph": "para\n- a\n- b\n\npara\n1. c\n",
  "task items": "- [ ] open\n- [x] done\n\n- [ ] loose\n",
  "a table": "before\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\n\nafter\n",
  "a table cut in its delimiter row": "| a | b |\n|--",
  "a table with no blank line after it": "| a |\n|---|\n| 1 |\nplain line\n\nend",
  "a table right after a paragraph": "para\n| a |\n|---|\n| 1 |\n\nend\n",
  "two tables": "| a |\n|---|\n| 1 |\n\n| b |\n|---|\n| 2 |\n",
  "a quote": "> one\n> two\n\n> three\nlazy\n\nafter\n\n> - list in a quote\n>\n>   more\n",
  "a quote holding a fence": "> ```\n> code\n>\n> more\n> ```\n\nend\n",
  "indented code after a paragraph": "para\n    not code\n\n    code\n\n    more code\n\nend",
  "indented code after a list": "- a\n\n      code in a\n\n    more\n\nend",
  "emphasis cut in the middle": "some **bold and `code` and [link](http://a.example/x",
  "emphasis across a blank line": "**a\n\nb**\n\n_c\n\nd_",
  "a link reference after its use": "see [the docs][d] and [other]\n\nmore text\n\n[d]: https://a.example/\n\nand [d] again\n",
  "a link reference in a quote": "see [x]\n\nmore\n\n> [x]: https://a.example/\n\nend [x]\n",
  "a link reference in a list": "see [x]\n\n- item\n\n  [x]: https://a.example/\n\nend\n",
  "images and links": "![alt](https://a.example/p.png)\n\n[![b](https://a.example/b.svg)](https://a.example/)\n\n<https://a.example/auto>\n\nhttps://a.example/bare\n",
  "a raw block left open": "first\n\n<div>\n\ninside\n\nstill inside\n\n</div>\n\nafter\n",
  "inline html left open": "first\n\nsome <b>bold\n\nnext block\n\n*last*\n",
  "a formatting element across blocks": "<em>a\n\nb\n\n</em>c\n\nd\n",
  "generics out of code": "A `Vec<u8>` and a Vec<u8> and Option<T>.\n\nNext paragraph.\n\nThird.\n",
  "an html comment": "a\n\n<!-- open\n\nstill in the comment\n\n-->\n\nb\n",
  "a pre": "a\n\n<pre>\n\nraw\n\n</pre>\n\nb\n",
  "a script": "a\n\n<script>alert(1)</script>\n\nb\n\n<img src=x onerror=alert(1)>\n\nc\n",
  "a fake code block": "a\n\n<pre class=\"code-block\" data-code-text=\"&quot;x&quot;\">y</pre>\n\nb\n",
  "crlf line ends": "one\r\n\r\ntwo\r\n\r\n```\r\ncode\r\n```\r\n\r\n- a\r\n- b\r\n",
  "a lone carriage return": "one\r\rtwo\n\nthree\n",
  "hidden characters": "a​b\n\n```\nx‮y z\n```\n\nend line\n",
  "wide characters": "你好，世界\n\n😀 **粗体** 😀\n\n```\n日本語\n```\n",
  "a hard break": "line one  \nline two\\\nline three\n\nnext\n",
  "escapes": "\\# not a heading\n\n\\- not a list\n\n&amp; &copy; &#35;\n",
};

/** Feeds `text` in batches of `size` and checks every step. */
function checkEverySplit(text: string, size: number) {
  let held: StreamedMarkdown | null = null;
  let settled = "";
  for (let end = 0; end < text.length; ) {
    end = Math.min(text.length, end + size);
    const prefix = text.slice(0, end);
    held = streamMarkdown(held, prefix);
    if (held.restarted) settled = "";
    settled += held.added;
    expect(settled + held.tail, JSON.stringify(prefix)).toBe(renderMarkdown(prefix));
  }
}

describe("a reply drawn in pieces is drawn as one rendering would draw it", () => {
  for (const [name, text] of Object.entries(AWKWARD)) {
    it(`${name}, at every character`, () => checkEverySplit(text, 1));
    it(`${name}, in batches`, () => {
      for (const size of [2, 3, 7, 16]) checkEverySplit(text, size);
    });
  }

  it("all of them one after another, in one reply", () => {
    // The pieces that tie blocks together stop the splitting, so they go last, one reply each.
    const tied = /link reference|raw|inline html|formatting|generics|comment|a pre|a script|fake|carriage|crlf/;
    const free = Object.entries(AWKWARD).filter(([name]) => !tied.test(name)).map(([, text]) => text);
    const reply = free.join("\n\n");
    for (const size of [5, 13, 40]) checkEverySplit(reply, size);
    for (const [name, text] of Object.entries(AWKWARD).filter(([name]) => tied.test(name))) {
      checkEverySplit(`${reply}\n\n${text}`, 37);
      expect(name).not.toBe("");
    }
  });

  it("keeps a link inside raw HTML past a paragraph the lexer folded a line into", () => {
    const text = "b\n    code\n|---|\n\n".repeat(3) + "first\n\n<div><a href=https://example.com>\n\nlink\n\n</a></div>\n\nafter\n";
    const held = streamMarkdown(null, text);
    expect(held.added + held.tail).toBe(renderMarkdown(text));
    expect(held.added + held.tail).toContain('href="https://example.com"');
  });

  it("a text that is not a continuation starts over", () => {
    const first = streamMarkdown(null, "one\n\ntwo\n\nthree\n");
    const second = streamMarkdown(first, "other\n");
    expect(second.restarted).toBe(true);
    expect(second.added + second.tail).toBe(renderMarkdown("other\n"));
  });

  it("the same text is no new step", () => {
    const first = streamMarkdown(null, "one\n\ntwo\n");
    expect(streamMarkdown(first, "one\n\ntwo\n")).toBe(first);
  });
});

describe("a long reply", () => {
  const block = (i: number) => `Paragraph ${i} with \`code\`.\n\n\`\`\`rust\nfn f${i}() {}\n\`\`\`\n\n- a ${i}\n- b\n\n| x |\n|---|\n| ${i} |\n\n`;

  it("parses only its open end: complete blocks are drawn once", () => {
    const text = Array.from({ length: 200 }, (_, i) => block(i)).join("");
    let held: StreamedMarkdown | null = null;
    let longestTail = 0;
    const before = markdownParseCount();
    let steps = 0;
    for (let end = 40; end < text.length; end += 40) {
      held = streamMarkdown(held, text.slice(0, end));
      longestTail = Math.max(longestTail, held.text.length - held.settled);
      steps += 1;
    }
    expect(longestTail).toBeLessThan(200);
    // Two passes at most per step: the blocks it completed and the open end.
    expect(markdownParseCount() - before).toBeLessThanOrEqual(steps * 2);
  });

  it("past raw HTML nothing is complete, and what came before it stays complete", () => {
    const before = Array.from({ length: 20 }, (_, i) => block(i)).join("");
    const text = `${before}Some <b>bold\n\n${block(21)}${block(22)}`;
    const held = streamMarkdown(streamMarkdown(null, before), text);
    expect(held.settled).toBeGreaterThan(before.length - 100);
    expect(held.settled).toBeLessThanOrEqual(before.length);
    expect(held.whole).toBe(false);
  });

  it("a link reference definition stops the splitting for good", () => {
    const start = `${block(1)}${block(2)}`;
    const held = streamMarkdown(streamMarkdown(null, start), `${start}[x]: https://a.example/\n\nmore\n`);
    expect(held.whole).toBe(true);
    expect(held.restarted).toBe(true);
    const next = streamMarkdown(held, `${held.text}and more\n`);
    expect(next.whole).toBe(true);
    expect(next.tail).toBe(renderMarkdown(next.text));
  });
});
