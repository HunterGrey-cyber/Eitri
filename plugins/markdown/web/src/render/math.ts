import katexPluginModule from "@vscode/markdown-it-katex";
import katex from "katex";
import type MarkdownIt from "markdown-it";
import { NONCE_ATTR } from "./env";
import { cjsDefault } from "./interop";

type RenderRule = NonNullable<MarkdownIt["renderer"]["rules"]["fence"]>;

/* A formula is input from the document: no command may make a link, load an image or set an id, class or style of
   its own (`trust: false`), a macro may not expand without end (`maxExpand`), and a size is capped in em. One that
   fails shows its source rather than stopping the rest of the document (`throwOnError: false`). */
export const KATEX_OPTIONS: {
  throwOnError: false; trust: false; maxExpand: 1000; maxSize: number; output: "htmlAndMathml"; strict: "ignore";
} = { throwOnError: false, trust: false, maxExpand: 1000, maxSize: 20, output: "htmlAndMathml", strict: "ignore" };

const katexPlugin = cjsDefault<(md: MarkdownIt, options?: object) => void>(katexPluginModule);

/* Environments KaTeX draws only in display mode; the plugin's own renderer switches an inline formula holding one. */
const DISPLAY_ONLY = /\\begin\{(align|equation|gather|cd|alignat)\}/i;

/* Rendered formulas by mode and source: retyping a paragraph re-renders the same formulas, and KaTeX is the slow
   part of a render. Bounded so a long session does not keep every formula it ever saw. */
const CACHE_LIMIT = 2000;
const cache = new Map<string, string>();

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string);
}

function nonceOf(env: unknown): string {
  return (env as { nonce: string }).nonce;
}

function placeholder(tag: "span" | "div", mode: "inline" | "display", source: string, nonce: string): string {
  return `<${tag} class="mdp-math" data-eitri-math="${mode}" ${NONCE_ATTR}="${nonce}">${escapeHtml(source)}</${tag}>`;
}

/** A formula waiting for `fillMath`: the source as text in a renderer-marked element, an inline `<span>` or a display
 *  `<div>`. Nothing is rendered before the sanitiser has run, so KaTeX's own markup never passes through it. */
export function mathPlaceholder(source: string, display: boolean, nonce: string): string {
  return display ? placeholder("div", "display", source, nonce) : placeholder("span", "inline", source, nonce);
}

/** The display placeholder for a block token (`$$` lines, or a fence whose language is `math`), carrying every
 *  attribute the token holds, the source lines among them. */
export const renderMathBlock: RenderRule = (tokens, idx, _options, env, slf) => {
  const token = tokens[idx];
  token.attrJoin("class", "mdp-math");
  token.attrSet("data-eitri-math", "display");
  token.attrSet(NONCE_ATTR, nonceOf(env));
  return `<div${slf.renderAttrs(token)}>${escapeHtml(token.content)}</div>\n`;
};

/** GitHub's math: `$...$` and `` $`...`$ `` inline, `$$...$$` as display math. Each becomes a placeholder. */
export function mathPlugin(md: MarkdownIt): void {
  md.use(katexPlugin, { enableFencedBlocks: false, enableBareBlocks: false });
  md.renderer.rules.math_inline = (tokens, idx, _options, env) => {
    const content = tokens[idx].content;
    // `` $`x`$ ``: the backticks are part of the delimiter, not of the formula.
    const source = content.length > 2 && content.startsWith("`") && content.endsWith("`") ? content.slice(1, -1) : content;
    return DISPLAY_ONLY.test(source)
      ? placeholder("span", "display", source, nonceOf(env))
      : placeholder("span", "inline", source, nonceOf(env));
  };
  // `$$x$$` in the middle of a paragraph arrives as an inline `math_block` token (no source lines of its own):
  // display mode, but a span, since a paragraph cannot hold a div.
  md.renderer.rules.math_block = (tokens, idx, options, env, slf) =>
    tokens[idx].map ? renderMathBlock(tokens, idx, options, env, slf)
      : placeholder("span", "display", tokens[idx].content, nonceOf(env));
}

function errorHtml(source: string, message: string): string {
  return `<span class="mdp-math-error" title="${escapeHtml(message)}">${escapeHtml(source)}</span>`;
}

function renderMath(source: string, display: boolean): string {
  const key = `${display ? "D" : "I"}\u0000${source}`;
  const hit = cache.get(key);
  if (hit !== undefined) return hit;
  let html: string;
  try {
    html = katex.renderToString(source, { ...KATEX_OPTIONS, displayMode: display });
    // KaTeX draws a parse error as one red span titled with its message; the preview shows the source instead,
    // underlined in its own error colour, with the same message on hover.
    const probe = document.createElement("span");
    probe.innerHTML = html;
    const only = probe.firstElementChild;
    if (probe.childElementCount === 1 && only?.classList.contains("katex-error")) {
      html = errorHtml(source, only.getAttribute("title") ?? "");
    }
  } catch (error) {
    html = errorHtml(source, error instanceof Error ? error.message : String(error));
  }
  if (cache.size >= CACHE_LIMIT) {
    const oldest = cache.keys().next();
    if (!oldest.done) cache.delete(oldest.value);
  }
  cache.set(key, html);
  return html;
}

/** Renders every placeholder under `root` not filled yet, in place; returns how many it filled. It runs on the
 *  sanitised fragment, so KaTeX's inline styles (the heights its layout needs) are kept. */
export function fillMath(root: ParentNode): number {
  const waiting = [...root.querySelectorAll<HTMLElement>("[data-eitri-math]:not([data-mdp-filled])")];
  for (const el of waiting) {
    const display = el.getAttribute("data-eitri-math") === "display";
    el.innerHTML = renderMath(el.textContent ?? "", display);
    el.setAttribute("data-mdp-filled", "");
  }
  return waiting.length;
}
