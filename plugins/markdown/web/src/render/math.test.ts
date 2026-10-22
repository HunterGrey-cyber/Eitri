import katex from "katex";
import MarkdownIt from "markdown-it";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fillMath, KATEX_OPTIONS, mathPlaceholder, mathPlugin, renderMathBlock } from "./math";

function md(): MarkdownIt {
  const m = new MarkdownIt();
  mathPlugin(m);
  return m;
}

function render(src: string, m: MarkdownIt = md()): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = m.render(src, { nonce: "n1" });
  return host;
}

afterEach(() => vi.restoreAllMocks());

describe("the options", () => {
  it("are the ones the preview promises", () => {
    expect(KATEX_OPTIONS).toEqual({
      throwOnError: false, trust: false, maxExpand: 1000, maxSize: 20, output: "htmlAndMathml", strict: "ignore",
    });
  });
});

describe("mathPlugin", () => {
  it("turns each delimiter into a placeholder holding the source as text", () => {
    const inline = render("a $x^2$ b").querySelector('span[data-eitri-math="inline"]')!;
    expect(inline.textContent).toBe("x^2");
    expect(inline.getAttribute("data-eitri-n")).toBe("n1");
    expect(inline.classList.contains("mdp-math")).toBe(true);
    expect(render("a $`x^2`$ b").querySelector('[data-eitri-math="inline"]')!.textContent).toBe("x^2");
    const block = render("$$\nx^2\n$$\n").querySelector('div[data-eitri-math="display"]')!;
    expect(block.textContent!.trim()).toBe("x^2");
    expect(block.getAttribute("data-eitri-n")).toBe("n1");
    expect(render("$$x^2$$\n").querySelector('div[data-eitri-math="display"]')!.textContent!.trim()).toBe("x^2");
    const inPara = render("a $$x$$ b").querySelector("p")!;
    expect(inPara.querySelector('span[data-eitri-math="display"]')!.textContent).toBe("x");
  });

  it("puts an inline formula that needs display mode in display mode", () => {
    expect(render("a $\\begin{align}a&=b\\end{align}$ b").querySelector('span[data-eitri-math="display"]'))
      .not.toBeNull();
  });

  it("leaves dollar amounts and math fences alone", () => {
    const host = render("price $5 and $6\n\n~~~math\nx\n~~~\n");
    expect(host.querySelector("[data-eitri-math]")).toBeNull();
    expect(host.querySelector("p")!.textContent).toBe("price $5 and $6");
    expect(host.querySelector("pre code.language-math")).not.toBeNull();
  });

  it("escapes the source", () => {
    const span = render("$a<b$").querySelector("[data-eitri-math]")!;
    expect(span.textContent).toBe("a<b");
    expect(span.querySelector("b")).toBeNull();
    expect(mathPlaceholder("a<b&", false, "n")).toBe(
      '<span class="mdp-math" data-eitri-math="inline" data-eitri-n="n">a&lt;b&amp;</span>');
    expect(mathPlaceholder("x", true, "n")).toBe(
      '<div class="mdp-math" data-eitri-math="display" data-eitri-n="n">x</div>');
  });

  it("writes every attribute a later core rule puts on a math block, in one class attribute", () => {
    const m = md();
    m.core.ruler.push("tag", (state) => {
      for (const t of state.tokens) {
        if (t.type === "math_block") { t.attrSet("data-line", String(t.map![0])); t.attrJoin("class", "code-line"); }
      }
    });
    const block = render("text\n\n$$\nx\n$$\n", m).querySelector("div[data-eitri-math]")!;
    expect(block.getAttribute("data-line")).toBe("2");
    expect([...block.classList].sort()).toEqual(["code-line", "mdp-math"]);
  });

  it("renders a math fence through renderMathBlock with the same shape", () => {
    const m = new MarkdownIt();
    m.renderer.rules.fence = (tokens, idx, options, env, slf) =>
      tokens[idx].info.trim() === "math" ? renderMathBlock(tokens, idx, options, env, slf) : "";
    const div = render("~~~math\nx<y\n~~~\n", m).querySelector('div[data-eitri-math="display"]')!;
    expect(div.textContent!.trim()).toBe("x<y");
    expect(div.getAttribute("data-eitri-n")).toBe("n1");
  });
});

describe("fillMath", () => {
  function holder(html: string): HTMLElement {
    const root = document.createElement("div");
    root.innerHTML = html;
    return root;
  }

  it("fills every placeholder with KaTeX's HTML and MathML and counts them", () => {
    const root = holder(mathPlaceholder("a+b", false, "n") + mathPlaceholder("\\sum_i x_i", true, "n"));
    expect(fillMath(root)).toBe(2);
    const [inline, display] = root.querySelectorAll("[data-eitri-math]");
    expect(inline.querySelector(".katex")).not.toBeNull();
    expect(inline.querySelector(".katex-mathml")).not.toBeNull();
    expect(display.querySelector(".katex-display")).not.toBeNull();
    expect(inline.hasAttribute("data-mdp-filled")).toBe(true);
  });

  it("shows a broken formula as its source with KaTeX's message, and the next one still renders", () => {
    const root = holder(mathPlaceholder("\\frac{", false, "n") + mathPlaceholder("y", false, "n"));
    fillMath(root);
    const error = root.querySelector(".mdp-math-error")!;
    expect(error.textContent).toBe("\\frac{");
    expect(error.getAttribute("title")).toContain("ParseError");
    expect(error.hasAttribute("style")).toBe(false);
    expect(root.querySelectorAll("[data-eitri-math]")[1].querySelector(".katex")).not.toBeNull();
  });

  it("renders each source once per mode, and a filled placeholder never again", () => {
    const spy = vi.spyOn(katex, "renderToString");
    const source = "\\alpha_{cache-test}";
    const root = holder(mathPlaceholder(source, false, "n") + mathPlaceholder(source, false, "n"));
    expect(fillMath(root)).toBe(2);
    expect(spy).toHaveBeenCalledTimes(1);
    expect(fillMath(root)).toBe(0);
    expect(spy).toHaveBeenCalledTimes(1);
    fillMath(holder(mathPlaceholder(source, true, "n")));
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("gives no TeX command a link, an image, a URL or a class of the document's choosing", () => {
    const root = holder([
      "\\href{javascript:alert(1)}{x}", "\\url{https://e.invalid}", "\\includegraphics{https://e.invalid/x.png}",
      "\\htmlClass{evil}{y}", "\\htmlId{evil}{y}", "\\htmlData{x=1}{y}",
    ].map((s) => mathPlaceholder(s, false, "n")).join(""));
    fillMath(root);
    expect(root.querySelector("a, img, [href], [src], .evil, #evil, [data-x]")).toBeNull();
  });
});
