import { describe, expect, it } from "vitest";
import { applyUpdate } from "./update";

function fragment(html: string): DocumentFragment {
  const t = document.createElement("template");
  t.innerHTML = html;
  return t.content;
}

describe("applyUpdate", () => {
  it("keeps equal elements, moving only their source lines, and replaces changed ones", () => {
    const live = document.createElement("main");
    live.innerHTML = '<p class="code-line" data-line="0" data-line-end="1">a</p>' +
      '<ul class="code-line" data-line="2" data-line-end="4"><li class="code-line" data-line="2" data-line-end="3">x</li></ul>' +
      '<p class="code-line" data-line="5" data-line-end="6">b</p>';
    const a = live.children[0];
    applyUpdate(live, fragment('<p class="code-line" data-line="0" data-line-end="1">a</p>' +
      '<p class="code-line" data-line="2" data-line-end="3">new</p>' +
      '<ul class="code-line" data-line="4" data-line-end="6"><li class="code-line" data-line="4" data-line-end="5">x</li></ul>' +
      '<p class="code-line" data-line="7" data-line-end="8">b changed</p>'));
    expect(live.children[0]).toBe(a);
    expect([...live.children].map((c) => [c.tagName, c.getAttribute("data-line"), c.textContent])).toEqual([
      ["P", "0", "a"], ["P", "2", "new"], ["UL", "4", "x"], ["P", "7", "b changed"],
    ]);
    expect(live.querySelector("li")!.getAttribute("data-line")).toBe("4");
  });

  it("keeps a drawn diagram whose source and theme are unchanged, and an image whose source is unchanged", () => {
    const live = document.createElement("main");
    live.innerHTML = '<div class="code-line mdp-diagram" data-line="0" data-line-end="3" data-mdp-key="1:A"><svg><g></g></svg></div>' +
      '<p class="code-line" data-line="4" data-line-end="5"><img src="x.png" alt="x"></p>';
    const svg = live.querySelector("svg")!;
    const img = live.querySelector("img")!;
    applyUpdate(live, fragment('<div class="code-line mdp-diagram" data-line="1" data-line-end="4" data-mdp-key="1:A"><svg><g></g></svg></div>' +
      '<p class="code-line" data-line="5" data-line-end="6"><img src="x.png" alt="x"> more</p>'));
    expect(live.querySelector("svg")).toBe(svg);
    expect(live.querySelector(".mdp-diagram")!.getAttribute("data-line")).toBe("1");
    expect(live.querySelector("img")).toBe(img);
  });

  it("replaces an element whose text moved around an unchanged child", () => {
    const live = document.createElement("main");
    live.innerHTML = '<p class="code-line" data-line="0" data-line-end="1">a <strong>a</strong></p>';
    applyUpdate(live, fragment('<p class="code-line" data-line="0" data-line-end="1"><strong>a</strong> a</p>'));
    expect(live.querySelector("p")!.innerHTML).toBe("<strong>a</strong> a");
  });

  it("keeps a details the reader opened open", () => {
    const live = document.createElement("main");
    live.innerHTML = '<details class="front-matter" data-line="0" data-line-end="3" open><summary>Front matter</summary><pre>a: 1</pre></details>';
    applyUpdate(live, fragment('<details class="front-matter" data-line="0" data-line-end="3"><summary>Front matter</summary><pre>a: 2</pre></details>'));
    expect(live.querySelector("details")!.hasAttribute("open")).toBe(true);
    expect(live.querySelector("pre")!.textContent).toBe("a: 2");
  });
});
