import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../../../..");
const read = (name: string) => readFileSync(resolve(here, name), "utf8");
const FILES = ["markdown.css", "hljs.css", "alerts.css", "chrome.css"];
const css = Object.fromEntries(FILES.map((f) => [f, read(f)]));
const all = FILES.map((f) => css[f]).join("\n");
const withoutComments = (text: string) => text.replace(/\/\*[\s\S]*?\*\//g, "");
const squash = (text: string) => withoutComments(text).replace(/\s+/g, " ").trim();
function rules(text: string): { selector: string; body: string }[] {
  return [...withoutComments(text).matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ selector: m[1].trim(), body: m[2] }));
}

describe("the page's stylesheets", () => {
  it("open with VS Code's notice, which the licence check finds in the build", () => {
    expect(css["markdown.css"].split("\n")[0]).toBe(
      "/*! Adapted from Visual Studio Code's Markdown preview (extensions/markdown-language-features), Copyright (c) Microsoft Corporation, MIT License. */");
  });

  it("use no VS Code variable, no desktop colour scheme, and only names the host sends", () => {
    expect(all).not.toContain("--vscode-");
    expect(all).not.toContain("prefers-color-scheme");
    const theme = JSON.parse(readFileSync(resolve(root, "core/src/plugin/fixtures/ev-theme.json"), "utf8"));
    const sent = new Set(Object.keys(theme.data.vars));
    const used = new Set([...withoutComments(all).matchAll(/var\((--nv-[a-z0-9-]+)/g)].map((m) => m[1]));
    expect(used.size).toBeGreaterThan(20);
    // The editor's text size comes in its own var, from a window with an editor of its own, and is optional.
    expect([...used].filter((name) => !sent.has(name) && name !== "--nv-text-size")).toEqual([]);
    expect(used.has("--nv-link")).toBe(true);
  });

  it("colour the page's background and text together, from the theme", () => {
    for (const selector of ["html", ".mdp-root"]) {
      const rule = rules(css["markdown.css"]).find((r) => r.selector === selector)!;
      expect(rule, selector).toBeDefined();
      expect(rule.body, selector).toMatch(/background-color:\s*var\(--nv-bg\)/);
      expect(rule.body, selector).toMatch(/(^|[^-])color:\s*var\(--nv-fg\)/);
    }
    expect(rules(css["markdown.css"]).find((r) => r.selector === "html")!.body).toMatch(/color-scheme:\s*var\(--nv-color-scheme\)/);
    expect(rules(css["markdown.css"]).find((r) => r.selector === "html")!.body).toMatch(/scroll-padding-top:\s*var\(--mdp-reserve, 110px\)/);
    expect(rules(css["markdown.css"]).find((r) => r.selector === "html")!.body).toMatch(/scroll-padding-bottom:\s*20px/);
    expect(rules(css["markdown.css"]).find((r) => r.selector === "html, body")!.body).toMatch(/font-size:\s*var\(--mdp-text\)/);
  });

  it("size the text from one base: the editor's size, else 14 px, and every length a ratio of it", () => {
    const html = rules(css["markdown.css"]).find((r) => r.selector === "html")!.body;
    expect(html).toMatch(/--mdp-text:\s*var\(--nv-text-size, 14px\)/);
    // 22 px of line at 14 px of text, as it was before the size could move.
    expect(html).toMatch(/--mdp-line:\s*calc\(var\(--mdp-text\) \* 11 \/ 7\)/);
    const base = rules(css["markdown.css"]).find((r) => r.selector === "html, body")!.body;
    expect(base).toMatch(/line-height:\s*var\(--mdp-line\)/);
    expect(squash(css["markdown.css"])).toContain("margin-bottom: calc(100vh - var(--mdp-line))");
    // Nothing else in the page's own sheets fixes a font size or a line height in px: those would not move.
    const fixed = rules(all).flatMap(({ selector, body }) =>
      body.split(";").map((d) => d.trim()).filter((d) => /^(font-size|line-height)\s*:\s*[\d.]+px/.test(d)).map((d) => `${selector} { ${d} }`));
    expect(fixed).toEqual([]);
  });

  it("never draw text in a signal colour; those colour borders, fills and glyphs", () => {
    const signal = /var\(--(nv-warn|nv-error|nv-ok|nv-chrome-accent|nv-focus-ring|mdp-alert)\b/;
    for (const { selector, body } of rules(all)) {
      for (const decl of body.split(";")) {
        const [prop, value = ""] = decl.split(":").map((s) => s.trim());
        if (prop === "color" && signal.test(value)) expect(selector, `${selector} { ${decl} }`).toMatch(/::before$/);
      }
    }
  });

  it("give each alert kind its signal colour on the bar and the glyph", () => {
    const alertRules = rules(css["alerts.css"]);
    for (const [kind, token] of [
      ["note", "--nv-chrome-accent"], ["tip", "--nv-ok"], ["important", "--nv-focus-ring"],
      ["warning", "--nv-warn"], ["caution", "--nv-error"],
    ]) {
      const rule = alertRules.find((r) => r.selector === `.markdown-alert-${kind}`)!;
      expect(rule, kind).toBeDefined();
      expect(rule.body, kind).toContain(`--mdp-alert: var(${token})`);
    }
    expect(squash(css["alerts.css"])).toContain("border-left: 0.25em solid var(--mdp-alert)");
  });

  it("copy the agent panel's highlight mapping", () => {
    const panel = readFileSync(resolve(root, "agent-ui/web/src/index.css"), "utf8");
    const start = panel.indexOf("/* Syntax keeps the editor's language palette. */");
    const end = panel.indexOf("@media (max-width: 560px)", start);
    expect(start).toBeGreaterThan(0);
    expect(squash(css["hljs.css"])).toContain(squash(panel.slice(start, end)));
    expect(squash(css["hljs.css"])).toContain(".hljs-function, .hljs-params { color: var(--nv-syn-variable); }");
    expect(squash(css["hljs.css"])).toContain(".hljs-title.class_ { color: var(--nv-syn-type); }");
  });

  it("style every class the page's modules draw", () => {
    for (const cls of [
      ".mdp-root", ".mdp-notice", ".mdp-box", ".mdp-box-reason", ".mdp-error-badge", ".mdp-math-error", ".mdp-diagram",
      ".mdp-diagram-pending", ".mdp-cursor-bar", ".mdp-current-bar", ".mdp-wide", ".front-matter", ".markdown-alert",
      ".markdown-alert-title", ".task-list-item",
    ]) {
      expect(all, cls).toContain(cls);
    }
  });

  it("leave search highlighting to the kit", () => {
    for (const [file, text] of Object.entries(css)) expect(text, file).not.toMatch(/mdp-search/);
  });
});
