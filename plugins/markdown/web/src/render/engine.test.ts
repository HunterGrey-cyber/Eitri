import { describe, expect, it } from "vitest";
import { createEngine } from "./engine";
import type { RenderEnv } from "./env";

const FENCE = "`".repeat(3);
const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
// Line numbers (0-based) in the comments: the tests below cite them.
const SAMPLE = [
  "---", "a: 1", "---",                               // 0-2 front matter
  "# T",                                              // 3
  "",
  "| a | b |", "|---|---|", "| 1 | 2 |",              // 5-7 table, rows at 5 and 7
  "",
  "- [x] done", "- [ ] todo",                         // 9, 10
  "",
  "> [!NOTE]", "> n",                                 // 12-13 alert
  "",
  "<div>", "raw", "</div>",                           // 15-17 raw HTML
  "",
  `${FENCE}rust`, "fn x() {}", FENCE,                 // 19-21
  "",
  `${FENCE}mermaid`, "graph TD", FENCE,               // 23-25
  "",
  "$$", "x", "$$",                                    // 27-29
  "",
  "---",                                              // 31 hr
  "",
  "t[^1] ~~s~~ https://example.org ![i](x.png) [l](tel:1)", // 33
  "",
  "[^1]: note",                                       // 35
  "",
  "# T",                                              // 37
].join("\n");

function env(over: Partial<RenderEnv> = {}): RenderEnv {
  return { nonce: "N", doc: DOC, remoteImages: "https", blocked: [], counts: {}, ...over };
}

function render(src: string, e: RenderEnv = env(), extend?: Parameters<typeof createEngine>[0]): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = createEngine(extend).render(src, e);
  return host;
}

const lines = (el: Element | null) => [el?.getAttribute("data-line"), el?.getAttribute("data-line-end")];

describe("the source map", () => {
  it("tags every block kind with its source lines, rows and items one by one, and closes with the end sentinel", () => {
    const host = render(SAMPLE);
    expect(lines(host.querySelector("details.front-matter"))).toEqual(["0", "3"]);
    expect(lines(host.querySelector("h1"))).toEqual(["3", "4"]);
    expect([...host.querySelectorAll("tr")].map((tr) => tr.getAttribute("data-line"))).toEqual(["5", "7"]);
    expect([...host.querySelectorAll("li.task-list-item")].map((li) => li.getAttribute("data-line"))).toEqual(["9", "10"]);
    expect(lines(host.querySelector(".markdown-alert"))).toEqual(["12", "14"]);
    const marker = host.querySelector('div.code-line[data-line="15"]')!;
    expect([marker.getAttribute("data-line-end"), marker.childNodes.length, marker.nextElementSibling!.textContent!.trim()])
      .toEqual(["18", 0, "raw"]);
    expect(lines(host.querySelector("pre.code-line code.language-rust")!.parentElement)).toEqual(["19", "22"]);
    expect(lines(host.querySelector("[data-eitri-mermaid]"))).toEqual(["23", "26"]);
    expect(lines(host.querySelector('[data-eitri-math="display"]'))).toEqual(["27", "30"]);
    expect(lines(host.querySelector("hr.code-line"))).toEqual(["31", "32"]);
    expect(host.querySelector(".footnote-item p")!.getAttribute("data-line")).toBe("35");
    expect(host.querySelectorAll("h1")[1].getAttribute("data-line")).toBe("37");
    const last = host.lastElementChild!;
    expect([last.className, last.getAttribute("data-line"), last.hasAttribute("data-line-end"), last.childNodes.length])
      .toEqual(["code-line", String(SAMPLE.split("\n").length), false, 0]);
    for (const el of host.querySelectorAll(".code-line")) {
      expect(el.getAttribute("data-eitri-n"), el.outerHTML.slice(0, 60)).toBe("N");
      expect(el.hasAttribute("data-line"), el.outerHTML.slice(0, 60)).toBe(true);
    }
  });
});

describe("line numbers", () => {
  it("keep the editor's lines when a line holds a carriage return no newline follows", () => {
    const host = render("a\rb\n\n# H\n\nx\r\r\ny");
    expect(lines(host.querySelector("p"))).toEqual(["0", "1"]);
    expect(host.querySelector("p")!.textContent).toBe("a b");
    expect(host.querySelector("h1")!.getAttribute("data-line")).toBe("2");
    expect(lines(host.querySelectorAll("p")[1])).toEqual(["4", "6"]);
    expect(host.lastElementChild!.getAttribute("data-line")).toBe("6");
  });
});

describe("the renderers", () => {
  it("highlight a known fence, escape an unknown one, and leave diagrams and math as placeholders", () => {
    const host = render(`${FENCE}rust\nfn x() {}\n${FENCE}\n\n${FENCE}nope\n<b>x</b>\n${FENCE}\n\n` +
      `${FENCE}mermaid\ngraph TD; A-->B\n${FENCE}\n\n~~~math\nx^2\n~~~\n\n    indented\n`);
    const [rust, nope, indented] = host.querySelectorAll("pre");
    expect(rust.classList.contains("mdp-wide")).toBe(true);
    expect(rust.querySelector("code")!.className).toBe("hljs language-rust");
    expect(rust.querySelector("span.hljs-keyword")!.getAttribute("data-eitri-n")).toBe("N");
    expect(nope.querySelector("code")!.hasAttribute("class")).toBe(false);
    expect(nope.querySelector("b")).toBeNull();
    expect(nope.textContent).toBe("<b>x</b>\n");
    const diagram = host.querySelector("[data-eitri-mermaid]")!;
    expect([...diagram.classList]).toEqual(expect.arrayContaining(["mdp-diagram", "mdp-diagram-pending", "mdp-wide"]));
    expect(diagram.textContent).toBe("graph TD; A-->B\n");
    expect(host.querySelector('div[data-eitri-math="display"]')!.textContent!.trim()).toBe("x^2");
    expect(indented.classList.contains("mdp-wide")).toBe(true);
    expect(indented.textContent).toBe("indented\n");
    expect(host.querySelector("table")).toBeNull();
  });

  it("resolve images against the document and box the ones it may not load", () => {
    const e = env();
    const host = render("![a](x.png) ![b](http://example.org/b.png) ![c](https://example.org/c.png)", e);
    expect([...host.querySelectorAll("img")].map((i) => [i.getAttribute("src"), i.getAttribute("alt")])).toEqual([
      [`${DOC.base}x.png`, "a"], ["https://example.org/c.png", "c"],
    ]);
    expect(e.blocked).toEqual([{ reason: "http", alt: "b" }]);
    const unsaved = env({ doc: null });
    render("![d](x.png)", unsaved);
    expect(unsaved.blocked).toEqual([{ reason: "unsaved", alt: "d" }]);
    const off = env({ remoteImages: "off" });
    render("![e](https://example.org/e.png)", off);
    expect(off.blocked).toEqual([{ reason: "remote_off", alt: "e" }]);
  });

  it("mark each link by what following it does, and take another scheme's href away", () => {
    const e = env();
    const host = render("[a](https://example.org) [b](#setup) [c](other.md) [d](tel:1) <https://example.org/x> " +
      "https://example.org/y [j](javascript:alert(1))", e);
    const links = [...host.querySelectorAll("a")];
    expect(links.map((a) => [a.getAttribute("data-eitri-link"), a.getAttribute("href")])).toEqual([
      ["external", "https://example.org"], ["anchor", "#setup"], ["local", "other.md"], ["none", null],
      ["external", "https://example.org/x"], ["external", "https://example.org/y"],
    ]);
    expect(e.counts.autolink).toBe(2);
    expect(host.textContent).toContain("[j](javascript:alert(1))");
  });

  it("give headings GitHub's ids, numbering a repeat", () => {
    const host = render("# Hello *World*\n\n## `x` y\n\n# Hello World");
    expect([...host.querySelectorAll("h1, h2")].map((h) => h.id))
      .toEqual(["user-content-hello-world", "user-content-x-y", "user-content-hello-world-1"]);
  });

  it("read escaped characters and character references into a heading's id", () => {
    const host = render("# a\\_b\n\n# caf&eacute; &amp; co");
    expect([...host.querySelectorAll("h1")].map((h) => [h.id, h.textContent]))
      .toEqual([["user-content-a_b", "a_b"], ["user-content-café--co", "café & co"]]);
  });

  it("number footnotes with user-content ids both ways", () => {
    const host = render("a[^n] b[^n]\n\n[^n]: the note");
    expect([...host.querySelectorAll(".footnote-ref a")].map((a) => [a.getAttribute("href"), a.id])).toEqual([
      ["#user-content-fn1", "user-content-fnref1"], ["#user-content-fn1", "user-content-fnref1:1"],
    ]);
    expect(host.querySelector("li.footnote-item")!.id).toBe("user-content-fn1");
    expect([...host.querySelectorAll(".footnote-backref")].map((a) => a.getAttribute("href")))
      .toEqual(["#user-content-fnref1", "#user-content-fnref1:1"]);
    expect(host.querySelector("section.footnotes")!.getAttribute("data-eitri-n")).toBe("N");
  });

  it("draw task checkboxes disabled", () => {
    const boxes = [...render("- [x] a\n- [ ] b").querySelectorAll("input")];
    expect(boxes.map((b) => [b.type, b.disabled, b.checked, b.id, b.getAttribute("data-eitri-n")]))
      .toEqual([["checkbox", true, true, "", "N"], ["checkbox", true, false, "", "N"]]);
  });

  it("leave a block whose rendering threw as its source with an error badge, and render the rest", () => {
    const host = render("before\n\n---\n\nafter", env(), (md) => {
      md.renderer.rules.hr = () => { throw new Error("boom"); };
    });
    const box = host.querySelector(".mdp-box.code-line")!;
    expect(lines(box)).toEqual(["2", "3"]);
    expect(box.querySelector(".mdp-error-badge")).not.toBeNull();
    expect(box.querySelector("pre code")!.textContent).toBe("---");
    expect([...host.querySelectorAll("p")].map((p) => p.textContent)).toEqual(["before", "after"]);
  });
});
