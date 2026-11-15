import { JSDOM } from "../../../../../plugins/markdown/web/node_modules/jsdom/lib/api.js";
import { writeFileSync } from "node:fs";

const dom = new JSDOM("<!doctype html><body></body>");
for (const key of ["window", "document", "Element", "HTMLElement", "SVGElement", "Node", "DOMParser", "XMLSerializer", "CSSStyleSheet"]) {
  globalThis[key] = dom.window[key];
}
// Only CSS retention is exercised; jsdom supplies no SVG text geometry.
Object.defineProperty(dom.window.SVGElement.prototype, "getBBox", {
  value() { return { x: 0, y: 0, width: Math.max(40, (this.textContent?.length ?? 0) * 8), height: 24 }; },
});
Object.defineProperty(dom.window.SVGElement.prototype, "getComputedTextLength", {
  value() { return (this.textContent?.length ?? 0) * 8; },
});
const { default: mermaid } = await import("../../../../../plugins/markdown/web/node_modules/mermaid/dist/mermaid.esm.mjs");
mermaid.initialize({
  startOnLoad: false, securityLevel: "strict", htmlLabels: false, flowchart: { htmlLabels: false },
  theme: "base", suppressErrorRendering: true,
  themeVariables: {
    primaryColor: "#123456", primaryTextColor: "#abcdef", lineColor: "#abcdef", textColor: "#abcdef",
    mainBkg: "#123456", noteBkgColor: "#123456", noteTextColor: "#abcdef",
  },
});
for (const [name, source] of [
  ["flowchart", "flowchart LR\nA-->B"],
  ["sequence", "sequenceDiagram\nAlice->>Bob: Hello"],
  ["class", "classDiagram\nAnimal <|-- Duck"],
]) {
  const { svg } = await mermaid.render("fixture" + name, source);
  writeFileSync(new URL(`mermaid-11.17.2-${name}.svg`, import.meta.url), svg + "\n");
}
