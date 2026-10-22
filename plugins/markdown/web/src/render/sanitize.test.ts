import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { RAW_HTML_END } from "./env";
import { sanitizeDocument, sanitizeSvg, type SanitizeContext } from "./sanitize";

const corpus = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "fixtures/hostile.html"), "utf8");
const doc = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/docs/" };

function clean(html: string, over: Partial<SanitizeContext> = {}) {
  const ctx: SanitizeContext = { nonce: "N0NCE", doc, remoteImages: "https", blocked: [], ...over };
  const host = document.createElement("div");
  host.append(sanitizeDocument(html, ctx));
  document.body.append(host);
  return { host, ctx };
}

const FORBIDDEN = "script, style, link, meta, base, iframe, frame, object, embed, form, button, select, option, " +
  "textarea, svg, math, video, audio, source, track, picture, canvas, template, noscript, dialog";

describe("the document profile against the hostile corpus", () => {
  it("leaves nothing that runs, loads, frames, posts or restyles", () => {
    const { host } = clean(corpus);
    expect((window as { __mdpRan?: number }).__mdpRan).toBeUndefined();
    expect(host.querySelector(FORBIDDEN)).toBeNull();
    for (const el of host.querySelectorAll("*")) {
      if (el.closest(".mdp-box")) continue; // the page's own box for an image it refused
      for (const attr of el.getAttributeNames()) {
        expect(attr.startsWith("on"), `${el.tagName} ${attr}`).toBe(false);
        expect(["style", "srcset", "ping", "download", "formaction", "xlink:href", "background", "target",
          "popovertarget", "tabindex", "hidden", "data-line", "data-line-end", "data-eitri-n", "data-eitri-mermaid",
          "data-eitri-math", "class"].includes(attr), `${el.tagName} ${attr}`).toBe(false);
      }
    }
    for (const a of host.querySelectorAll("a[href]")) {
      expect(a.getAttribute("href")!, a.outerHTML).toMatch(/^(https?:|mailto:|#|[^a-z:]*$|[a-z0-9._~/-]+(\.[a-z]+)?$)/i);
    }
    for (const img of host.querySelectorAll("img")) {
      expect(img.getAttribute("src")!, img.outerHTML).toMatch(/^(eitri-plugin:\/\/markdown\/\.eitri\/doc\/4\/|https:|data:image\/)/);
    }
    for (const el of host.querySelectorAll("[id], [name]")) {
      for (const attr of ["id", "name"]) {
        if (el.hasAttribute(attr)) expect(el.getAttribute(attr)!, el.outerHTML).toMatch(/^user-content-/);
      }
    }
    host.remove();
  });

  it("survives attributes named after an object's own properties", () => {
    const { host } = clean('<p constructor="x" __proto__="y" hasOwnProperty="z" toString="w">proto</p>');
    const p = host.querySelector("p")!;
    expect([p.textContent, p.getAttributeNames()]).toEqual(["proto", []]);
    host.remove();
  });

  it("keeps what GitHub shows", () => {
    const { host } = clean(corpus);
    expect(host.querySelector("details[open] > summary")!.textContent).toBe("more");
    expect(host.querySelector("kbd")!.textContent).toBe("Ctrl");
    expect(host.querySelector("sub")!.textContent).toBe("2");
    expect(host.querySelector("abbr[title=HyperText]")).not.toBeNull();
    expect(host.querySelector('a[href="https://example.org/"]')!.textContent).toBe("ok");
    expect(host.textContent).toContain("styled");
    expect(host.querySelector("td")!.textContent).toBe("cell");
    host.remove();
  });
});

describe("renderer-marked attributes", () => {
  it("are kept only with this render's nonce, which is removed afterwards", () => {
    const { host } = clean(
      '<p class="code-line" data-line="1" data-line-end="2" data-eitri-n="N0NCE" id="user-content-h">ok</p>' +
      '<p class="code-line" data-line="3" data-eitri-n="WRONG">bad</p>' +
      '<p class="code-line" data-line="5">none</p>' +
      '<div class="mdp-diagram" data-eitri-mermaid="" data-eitri-n="N0NCE">g</div>' +
      '<span data-eitri-math="inline" data-eitri-n="other">m</span>');
    const [ok, bad, none] = host.querySelectorAll("p");
    expect([ok.className, ok.getAttribute("data-line"), ok.getAttribute("data-line-end"), ok.id])
      .toEqual(["code-line", "1", "2", "user-content-h"]);
    for (const p of [bad, none]) expect(p.getAttributeNames(), p.textContent!).toEqual([]);
    expect(host.querySelector("[data-eitri-mermaid]")!.className).toBe("mdp-diagram");
    expect(host.querySelector("[data-eitri-math]")).toBeNull();
    expect(host.querySelector("[data-eitri-n]")).toBeNull();
    host.remove();
  });

  it("are never kept by an empty nonce", () => {
    const { host } = clean('<p class="code-line" data-line="1" data-eitri-n="">x</p><p class="y">z</p>', { nonce: "" });
    for (const p of host.querySelectorAll("p")) expect(p.getAttributeNames(), p.textContent!).toEqual([]);
    host.remove();
  });

  it("stay the renderer's when raw HTML stops inside a tag, whatever state it stops in", () => {
    const next = '<p class="code-line" data-line="2" data-line-end="3" data-eitri-n="N0NCE">after</p>';
    const forged = '<div class="mdp-current-bar" data-line="999" data-eitri-x=""';
    for (const raw of [forged, `${forged} title="`, `${forged} title='`, `${forged} title=`, `${forged} title`,
      `${forged}/`, "<div", "</div", "<!--", "<!x", "<?x", "<!DOCTYPE x PUBLIC \"", "<"]) {
      const { host } = clean(`${raw}\n${RAW_HTML_END}${next}`);
      expect(host.querySelector(".mdp-current-bar, [data-line='999'], [data-eitri-x]"), raw).toBeNull();
      const p = host.querySelector("p")!;
      expect([p.className, p.getAttribute("data-line"), p.textContent], raw).toEqual(["code-line", "2", "after"]);
      host.remove();
    }
  });

  it("keep a task checkbox only when the renderer made it", () => {
    const { host } = clean('<input type="checkbox" checked disabled class="task-list-item-checkbox" data-eitri-n="N0NCE">' +
      '<input type="checkbox" checked><input type="text" data-eitri-n="N0NCE">');
    const inputs = host.querySelectorAll("input");
    expect(inputs.length).toBe(1);
    expect([inputs[0].type, inputs[0].checked, inputs[0].disabled, inputs[0].className])
      .toEqual(["checkbox", true, true, "task-list-item-checkbox"]);
    host.remove();
  });

  it("prefix a document's ids and anchor names, and keep a marked id as written", () => {
    const { host } = clean('<h2 id="setup">a</h2><a name="top">b</a><div name="x">c</div>' +
      '<h2 id="user-content-intro" data-eitri-n="N0NCE">d</h2>');
    expect(host.querySelector("h2")!.id).toBe("user-content-setup");
    expect(host.querySelector("a")!.getAttribute("name")).toBe("user-content-top");
    expect(host.querySelector("div")!.hasAttribute("name")).toBe(false);
    expect(host.querySelectorAll("h2")[1].id).toBe("user-content-intro");
    host.remove();
  });

  it("keep width and height only on img, open only on details, start and type only on ol", () => {
    const { host } = clean('<img src="x.png" width="3" height="4" alt="a"><div width="3" open>d</div>' +
      '<ol start="3" type="a"><li>l</li></ol><ul type="disc"><li>u</li></ul>');
    expect(host.querySelector("img")!.getAttribute("width")).toBe("3");
    expect(host.querySelector("div")!.getAttributeNames()).toEqual([]);
    expect(host.querySelector("ol")!.getAttribute("start")).toBe("3");
    expect(host.querySelector("ul")!.hasAttribute("type")).toBe(false);
    host.remove();
  });
});

describe("raw HTML images", () => {
  it("go through the same resolver as a Markdown image, the blocked ones as boxes counted in the context", () => {
    const { host, ctx } = clean('<p align="center"><img src="logo.png" alt="logo"> ' +
      '<img src="https://example.org/badge.svg" alt="ci"> <img src="http://example.org/b.png" alt="old"> ' +
      '<img src="../../../up.png" alt="up"></p>');
    const imgs = host.querySelectorAll("img");
    expect([...imgs].map((i) => i.getAttribute("src"))).toEqual([`${doc.base}logo.png`, "https://example.org/badge.svg"]);
    expect(ctx.blocked).toEqual([{ reason: "http", alt: "old" }, { reason: "outside", alt: "up" }]);
    const boxes = host.querySelectorAll(".mdp-box");
    expect([...boxes].map((b) => b.getAttribute("data-eitri-blocked"))).toEqual(["http", "outside"]);
    expect(boxes[0].querySelector(".mdp-box-reason")!.textContent).toBe("http image not loaded");
    host.remove();
  });

  it("box an https image too when remote images are off, and keep a marked image's source as the renderer wrote it", () => {
    const { host, ctx } = clean('<img src="https://example.org/a.png" alt="a">' +
      `<img src="${doc.base}x.png" alt="m" data-eitri-n="N0NCE">`, { remoteImages: "off" });
    expect(ctx.blocked).toEqual([{ reason: "remote_off", alt: "a" }]);
    expect(host.querySelector("img")!.getAttribute("src")).toBe(`${doc.base}x.png`);
    host.remove();
  });
});

describe("sanitizeSvg", () => {
  it("keeps Mermaid's drawing and classes and drops script, foreign content, images and handlers", () => {
    const host = document.createElement("div");
    host.append(sanitizeSvg('<svg viewBox="0 0 10 10" onload="alert(1)"><style>.node rect{fill:#fff}</style>' +
      '<g class="node"><rect width="5" height="5"/><text>A</text></g><foreignObject><div>h</div></foreignObject>' +
      '<script>alert(1)</script><image href="https://example.org/x.png"/><use href="#x"/>' +
      '<a href="javascript:alert(1)"><text>l</text></a>' +
      '<filter id="f"><feImage href="https://example.org/x.png"/><feGaussianBlur stdDeviation="1"/></filter></svg>'));
    const svg = host.querySelector("svg")!;
    expect(svg.hasAttribute("onload")).toBe(false);
    expect(svg.querySelector("g.node > rect")).not.toBeNull();
    expect(svg.querySelector("style")!.textContent).toContain(".node rect");
    expect(svg.querySelector("foreignObject, script, image, use, a, feImage")).toBeNull();
    expect(svg.querySelector("feGaussianBlur")).not.toBeNull();
  });
});
