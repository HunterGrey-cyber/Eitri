import MarkdownIt from "markdown-it";
import { describe, expect, it, vi } from "vitest";
import { frontMatterPlugin } from "./frontmatter";

const highlight = vi.fn((code: string, _lang: string, nonce: string) =>
  `<span data-eitri-n="${nonce}" class="hljs-attr">${new MarkdownIt().utils.escapeHtml(code)}</span>`);

function setup(h: (code: string, lang: string, nonce: string) => string | null = highlight) {
  const md = new MarkdownIt();
  frontMatterPlugin(md, h);
  return md;
}

function render(src: string, h?: (code: string, lang: string, nonce: string) => string | null): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = setup(h).render(src, { nonce: "n1" });
  return host;
}

describe("frontMatterPlugin", () => {
  it("shows YAML between --- lines at the top as a closed details holding highlighted code", () => {
    highlight.mockClear();
    const host = render("---\ntitle: x\ntags: [a]\n---\n# Heading\n");
    const details = host.firstElementChild!;
    expect(details.tagName).toBe("DETAILS");
    expect(details.classList.contains("front-matter")).toBe(true);
    expect(details.hasAttribute("open")).toBe(false);
    expect(details.getAttribute("data-eitri-n")).toBe("n1");
    const summary = details.querySelector(":scope > summary")!;
    expect(summary.textContent).toBe("Front matter");
    expect(summary.getAttribute("data-eitri-n")).toBe("n1");
    const code = details.querySelector("pre > code")!;
    expect([...code.classList]).toEqual(["hljs", "language-yaml"]);
    expect(code.getAttribute("data-eitri-n")).toBe("n1");
    expect(details.querySelector("pre")!.getAttribute("data-eitri-n")).toBe("n1");
    expect(code.textContent).toBe("title: x\ntags: [a]");
    expect(highlight).toHaveBeenCalledWith("title: x\ntags: [a]", "yaml", "n1");
    expect(host.querySelector("h1")!.textContent).toBe("Heading");
    const token = setup().parse("---\ntitle: x\ntags: [a]\n---\n# Heading\n", { nonce: "n1" })[0];
    expect([token.type, token.map]).toEqual(["front_matter", [0, 4]]);
  });

  it("also closes at a ... line and allows trailing spaces on both marker lines", () => {
    expect(render("---\na: 1\n...\nbody").querySelector("details.front-matter code")!.textContent).toBe("a: 1");
    expect(render("--- \na: 1\n---  \nbody").querySelector("details.front-matter code")!.textContent).toBe("a: 1");
  });

  it("takes an empty block", () => {
    expect(render("---\n---\nbody").querySelector("details.front-matter code")!.textContent).toBe("");
    expect(setup().parse("---\n---\nbody", { nonce: "n1" })[0].map).toEqual([0, 2]);
  });

  it("is front matter only at the very top, at column 0, with exactly three dashes, and closed", () => {
    for (const src of [
      "text\n\n---\na: 1\n---\n",
      "  ---\na: 1\n---\n",
      "----\na: 1\n----\n",
      "---\n# Title\n\nno closing line\n",
    ]) {
      expect(render(src).querySelector("details.front-matter"), JSON.stringify(src)).toBeNull();
    }
    const unclosed = render("---\n# Title\n");
    expect(unclosed.querySelector("hr")).not.toBeNull();
    expect(unclosed.querySelector("h1")!.textContent).toBe("Title");
  });

  it("escapes the YAML when the highlighter knows no YAML", () => {
    const code = render("---\na: <b>x</b>\n---\n", () => null).querySelector("details.front-matter code")!;
    expect(code.textContent).toBe("a: <b>x</b>");
    expect(code.querySelector("b")).toBeNull();
  });

  it("writes every attribute a later core rule puts on the token, in one class attribute", () => {
    const md = setup();
    md.core.ruler.push("tag", (state) => {
      for (const t of state.tokens) {
        if (t.type === "front_matter") { t.attrSet("data-line", String(t.map![0])); t.attrJoin("class", "code-line"); }
      }
    });
    const host = document.createElement("div");
    host.innerHTML = md.render("---\na: 1\n---\n", { nonce: "n1" });
    const details = host.firstElementChild!;
    expect(details.getAttribute("data-line")).toBe("0");
    expect([...details.classList].sort()).toEqual(["code-line", "front-matter"]);
  });
});
