import taskLists from "@hedgedoc/markdown-it-task-lists";
import GithubSlugger from "github-slugger";
import MarkdownIt from "markdown-it";
import footnote from "markdown-it-footnote";
import { alertsPlugin } from "./alerts";
import { FENCE_ATTR, mark, NONCE_ATTR, RAW_HTML_ATTR, RAW_HTML_END, type RenderEnv } from "./env";
import { frontMatterPlugin } from "./frontmatter";
import { highlightCode } from "./highlight";
import { boxHtml, resolveImage } from "./images";
import { classifyHref } from "./links";
import { mathPlugin, renderMathBlock } from "./math";
import { sourceMapPlugin } from "./sourcemap";

/** Turns a document's text into HTML for the sanitiser. One engine holds one configured markdown-it and is reused
 *  for every render; everything that differs per render travels in `env`. */
export interface Engine {
  render(text: string, env: RenderEnv): string;
}

type Token = ReturnType<MarkdownIt["parse"]>[number];

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string);
}

function nonceOf(env: unknown): string {
  return (env as RenderEnv).nonce;
}

function count(env: unknown, key: string): void {
  const counts = (env as RenderEnv).counts;
  counts[key] = (counts[key] ?? 0) + 1;
}

/* GitHub's heading ids: the heading's text (its plain and code parts, as GitHub reads it), slugged, numbered on a
 * repeat, and prefixed `user-content-` so an id can never shadow one of the page's own globals. The slugger is new
 * per parse, so a repeat is counted within one document only. Running after `inline` is what makes the heading's
 * children exist; it also runs before `text_join`, so a backslash escape or a character reference is still its own
 * `text_special` token (holding the character it stands for) and is read here too, or `# a\_b` would slug as `ab`
 * while the heading shows `a_b`. */
function headingIds(md: MarkdownIt): void {
  md.core.ruler.after("inline", "eitri_heading_ids", (state) => {
    const slugger = new GithubSlugger();
    for (let i = 0; i < state.tokens.length; i++) {
      const token = state.tokens[i];
      if (token.type !== "heading_open") continue;
      const text = (state.tokens[i + 1]?.children ?? [])
        .filter((c) => c.type === "text" || c.type === "text_special" || c.type === "code_inline")
        .map((c) => c.content)
        .join("");
      token.attrSet("id", `user-content-${slugger.slug(text)}`);
    }
    return false;
  });
}

/* Footnote numbers as the footnote plugin counts them: its ids start at 0, a reader's at 1, and a note cited more
 * than once gets `:1`, `:2` ... on each later citation. */
function footnoteIds(token: Token): { n: string; sub: string } {
  const meta = token.meta as { id: number; subId?: number };
  const sub = Number(meta.subId ?? 0) > 0 ? `:${meta.subId}` : "";
  return { n: String(Number(meta.id) + 1), sub };
}

function pageRules(md: MarkdownIt): void {
  const rules = md.renderer.rules;

  // Fences: math and diagrams become placeholders filled after sanitising, so neither KaTeX's nor Mermaid's markup
  // ever passes through the sanitiser; code is highlighted only for a language it names, never guessed. Each
  // writes the token's own attributes, which is how the source lines reach the element.
  rules.fence = (tokens, idx, options, env, slf) => {
    const token = tokens[idx];
    const lang = (token.info.trim().split(/\s+/)[0] ?? "").toLowerCase();
    if (lang === "math") return renderMathBlock(tokens, idx, options, env, slf);
    const nonce = nonceOf(env);
    if (lang === "mermaid") {
      // The diagram's box scrolls sideways like code does, rather than squeezing a wide drawing.
      token.attrJoin("class", "mdp-diagram mdp-diagram-pending mdp-wide");
      token.attrSet("data-eitri-mermaid", "");
      token.attrSet(NONCE_ATTR, nonce);
      return `<div${slf.renderAttrs(token)}>${escapeHtml(token.content)}</div>\n`;
    }
    const code = highlightCode(token.content, lang, nonce);
    token.attrJoin("class", "mdp-wide");
    token.attrSet(NONCE_ATTR, nonce);
    // The block's map holds its fence lines too; the code's own lines are the ones its rows show.
    const content = token.content === "" ? 0 : token.content.split("\n").length - (token.content.endsWith("\n") ? 1 : 0);
    token.attrSet(FENCE_ATTR, String(content));
    const cls = code === null ? "" : ` class="hljs language-${escapeHtml(lang)}"`;
    const body = code ?? escapeHtml(token.content);
    return `<pre${slf.renderAttrs(token)}><code${cls}${mark({ nonce })}>${body}</code></pre>\n`;
  };

  rules.code_block = (tokens, idx, _options, env, slf) => {
    const token = tokens[idx];
    token.attrJoin("class", "mdp-wide");
    token.attrSet(NONCE_ATTR, nonceOf(env));
    return `<pre${slf.renderAttrs(token)}><code>${escapeHtml(token.content)}</code></pre>\n`;
  };

  // A wide table scrolls on its own instead of widening the page.
  rules.table_open = (tokens, idx, options, env, slf) => {
    tokens[idx].attrJoin("class", "mdp-wide");
    tokens[idx].attrSet(NONCE_ATTR, nonceOf(env));
    return slf.renderToken(tokens, idx, options);
  };

  // Raw HTML is the document's, so no attribute of ours can go onto it: an empty marker in front of it carries its
  // source lines instead, and an end marker lets the index measure the rendered extent between them. The HTML
  // goes on to the sanitiser closed off so that it cannot run into the tag the renderer writes next.
  rules.html_block = (tokens, idx, _options, env) => {
    const token = tokens[idx];
    if (!token.map) return token.content + RAW_HTML_END;
    const [line, end] = token.map;
    const nonce = mark({ nonce: nonceOf(env) });
    const e = env as RenderEnv;
    const ordinal = e.rawHtmlBlocks ?? 0;
    e.rawHtmlBlocks = ordinal + 1;
    return `<div class="code-line" data-line="${line}" data-line-end="${end}" ${RAW_HTML_ATTR}="start:${ordinal}"${nonce}></div>` +
      token.content + RAW_HTML_END + `<span ${RAW_HTML_ATTR}="end:${ordinal}"${nonce}></span>`;
  };
  rules.html_inline = (tokens, idx) => tokens[idx].content + RAW_HTML_END;

  // Images are resolved here, against the document's place in the project, so the URL the page loads is decided
  // before any HTML exists; one that may not load becomes a box saying why, and is counted.
  rules.image = (tokens, idx, options, env, slf) => {
    const token = tokens[idx];
    const e = env as RenderEnv;
    const alt = slf.renderInlineAsText(token.children ?? [], options, env);
    const resolved = resolveImage(token.attrGet("src") ?? "", e.doc, e.remoteImages);
    if ("blocked" in resolved) {
      e.blocked.push({ reason: resolved.blocked, alt });
      return boxHtml(alt, resolved.blocked, e.nonce);
    }
    const title = token.attrGet("title");
    const titleAttr = title ? ` title="${escapeHtml(title)}"` : "";
    return `<img src="${escapeHtml(resolved.url)}" alt="${escapeHtml(alt)}"${titleAttr}${mark(e)}>`;
  };

  // Every link says what following it does, which the page's click handler reads; a link to a scheme the page
  // never follows loses its href, so nothing (not even WebKit's own navigation) can follow it.
  rules.link_open = (tokens, idx, options, env, slf) => {
    const token = tokens[idx];
    const kind = classifyHref(token.attrGet("href") ?? "");
    if (kind === "none") token.attrs = (token.attrs ?? []).filter(([name]) => name !== "href");
    token.attrSet("data-eitri-link", kind);
    if (token.markup === "linkify" || token.info === "auto") {
      token.attrSet("data-eitri-auto", "");
      count(env, "autolink");
    }
    token.attrSet(NONCE_ATTR, nonceOf(env));
    return slf.renderToken(tokens, idx, options);
  };

  // The preview reads; it never edits the document, so a checkbox is drawn disabled, and without the id the
  // plugin's own renderer writes (nothing labels it).
  rules.taskListItemCheckbox = (tokens, idx, _options, env) => {
    const checked = tokens[idx].attrGet("checked") ? " checked" : "";
    return `<input class="task-list-item-checkbox" type="checkbox" disabled${checked}${mark({ nonce: nonceOf(env) })}>`;
  };

  // The footnote plugin writes bare `fn1` ids with no way to prefix them; these write GitHub's `user-content-` ids
  // both ways, matching the prefix every other id on the page has.
  rules.footnote_ref = (tokens, idx, options, env, slf) => {
    const { n, sub } = footnoteIds(tokens[idx]);
    const m = mark({ nonce: nonceOf(env) });
    const caption = slf.rules.footnote_caption?.(tokens, idx, options, env, slf) ?? `[${n}${sub}]`;
    return `<sup class="footnote-ref"${m}><a href="#user-content-fn${n}" id="user-content-fnref${n}${sub}"${m}>` +
      `${caption}</a></sup>`;
  };
  rules.footnote_block_open = (_tokens, _idx, _options, env) => {
    const m = mark({ nonce: nonceOf(env) });
    return `<hr class="footnotes-sep"${m}>\n<section class="footnotes"${m}>\n<ol class="footnotes-list"${m}>\n`;
  };
  rules.footnote_open = (tokens, idx, _options, env) => {
    const { n, sub } = footnoteIds(tokens[idx]);
    return `<li id="user-content-fn${n}${sub}" class="footnote-item"${mark({ nonce: nonceOf(env) })}>`;
  };
  // U+FE0E keeps the arrow a text glyph rather than an emoji.
  rules.footnote_anchor = (tokens, idx, _options, env) => {
    const { n, sub } = footnoteIds(tokens[idx]);
    const m = mark({ nonce: nonceOf(env) });
    return ` <a href="#user-content-fnref${n}${sub}" class="footnote-backref"${m}>\u21a9\ufe0e</a>`;
  };
}

/* A block that could not be rendered still shows: its source lines as code, under a badge, at the lines it came
 * from, so the rest of the document and the scroll sync are unaffected. */
function errorBox(lines: string[], map: [number, number] | null, nonce: string): string {
  const m = mark({ nonce });
  const at = map ? ` data-line="${map[0]}" data-line-end="${map[1]}"` : "";
  const source = map ? lines.slice(map[0], map[1]).join("\n") : "";
  return `<div class="mdp-box code-line"${at}${m}>` +
    `<span class="mdp-error-badge"${m}>could not render this block</span>` +
    `<pre${m}><code>${escapeHtml(source)}</code></pre></div>\n`;
}

/* The top-level blocks of a token stream: an opening token with everything up to its matching close, or a single
 * token. Rendering them one at a time is what lets one block fail alone. */
function* blocks(tokens: Token[]): Generator<Token[]> {
  for (let i = 0; i < tokens.length; ) {
    let j = i;
    if (tokens[i].nesting === 1) {
      let depth = 0;
      for (; j < tokens.length; j++) {
        depth += tokens[j].nesting;
        if (depth === 0) break;
      }
    }
    yield tokens.slice(i, j + 1);
    i = j + 1;
  }
}

/** The page's markdown-it: raw HTML on (the sanitiser decides what of it stays), bare URLs linked but not bare
 *  domains (`fuzzyLink` would turn `README.md` into a link), no typographer and no hard breaks, as GitHub renders.
 *  The source map goes on last so it sees every token the other rules made. `extend` runs just before it, for
 *  tests that need a rule of their own. */
export function createEngine(extend?: (md: MarkdownIt) => void): Engine {
  const md = new MarkdownIt({ html: true, linkify: true, typographer: false, breaks: false });
  md.linkify.set({ fuzzyLink: false });
  frontMatterPlugin(md, highlightCode);
  // The footnote plugin's type declarations name markdown-it's older module layout, which TypeScript treats as a
  // different type from the one imported here; at run time it is the same plugin function.
  md.use(footnote as unknown as (md: MarkdownIt) => void);
  md.use(taskLists, { enabled: false, label: false, lineNumber: false });
  alertsPlugin(md);
  mathPlugin(md);
  headingIds(md);
  pageRules(md);
  extend?.(md);
  sourceMapPlugin(md);

  return {
    render(text, env) {
      env.rawHtmlBlocks = 0;
      const lines = text.split("\n");
      // The end sentinel stands for the line after the last, so a scroll position past every block still maps.
      const sentinel = `<div class="code-line" data-line="${lines.length}"${mark(env)}></div>\n`;
      // The text's lines are the editor's, split on `\n` alone, but markdown-it also ends a line at a `\r` that no
      // `\n` follows, which would number every later block one line further down than the editor does. Such a `\r`
      // is part of its line here (the editor shows it inside the line), so it is parsed as a space; `\r\n` stays,
      // as markdown-it counts it as the one line end it is.
      const parsed = text.replace(/\r(?!\n)/g, " ");
      let tokens: Token[];
      try {
        tokens = md.parse(parsed, env);
      } catch {
        return errorBox(lines, [0, lines.length], env.nonce) + sentinel;
      }
      let out = "";
      for (const group of blocks(tokens)) {
        try {
          out += md.renderer.render(group, md.options, env);
        } catch {
          const map = group.find((t) => t.map)?.map ?? null;
          out += errorBox(lines, map as [number, number] | null, env.nonce);
        }
      }
      return out + sentinel;
    },
  };
}
