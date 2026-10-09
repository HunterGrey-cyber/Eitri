import { marked, Renderer, type Tokens } from "marked";
import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import diff from "highlight.js/lib/languages/diff";
import go from "highlight.js/lib/languages/go";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import lua from "highlight.js/lib/languages/lua";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import toml from "highlight.js/lib/languages/ini";
import typescript from "highlight.js/lib/languages/typescript";
import yaml from "highlight.js/lib/languages/yaml";
import { escapeText, isHiddenChar } from "./revealHidden";

/* Register only the selected languages from `lib/core` to keep the single-file bundle small.
   `tsx` aliases typescript, and TOML uses highlight.js's `ini` grammar. */
for (const [name, language] of [
  ["rust", rust], ["typescript", typescript], ["javascript", javascript], ["python", python],
  ["go", go], ["lua", lua], ["bash", bash], ["json", json], ["toml", toml], ["yaml", yaml],
  ["diff", diff],
] as const) {
  hljs.registerLanguage(name, language);
}
hljs.registerAliases(["ts", "tsx"], { languageName: "typescript" });
hljs.registerAliases(["js", "jsx"], { languageName: "javascript" });
hljs.registerAliases(["sh", "shell", "zsh"], { languageName: "bash" });

/* Exported so `highlightCoverage.test.ts` can run the SAME registered highlighter this module uses
   internally, rather than a second copy of the registration
   block above drifting out of sync with this one. */
export { hljs };

/* marked 13's RENDERER TYPE is `code({ text, lang, escaped })` (`marked.d.ts:172`), but by default
   `marked.use()` still runs a `code` renderer through a legacy compatibility shim that calls it
   with the OLD positional args (`func.call(this, token.text, token.lang, !!token.escaped)` --
   `marked.esm.js`'s `#convertRendererFunction`), destructuring an object out of a plain string and
   silently producing `text: undefined`. `useNewRenderer: true` is what actually turns the token on
   ("Use the new renderer that accepts an object instead of individual parameters. This option will
   be removed and default to true in the next major version." -- `marked.d.ts`'s own doc comment on
   the flag). Confirmed by reproducing the failure first: without this flag every fenced block in
   `markdown.test.ts` rendered as an empty `<pre><code></code></pre>`. Re-check both this flag and
   the token shape if marked is upgraded. */
/* Images inside link text, including badges and mixed content, must not render their own anchors.
   Nested anchors are split into siblings by the HTML parser, making the image text point at the
   image URL instead of the enclosing link. `linkDepth` tracks the synchronous inline parse so
   the image renderer can detect an enclosing link, even through emphasis.
   Save `defaultLink` before overriding it to retain marked's href cleaning and title handling.
   `Renderer.prototype.link` already accepts an object argument, unlike code's legacy shim. */
const defaultLink = Renderer.prototype.link;
let linkDepth = 0;

/* Which `<pre>` is a code block the renderer wrote, as opposed to one a reply's own raw HTML
   claims to be. The panel's copy and HINT code paths treat a `pre.code-block` as "the fenced block
   the reader is looking at", so that class must be something only the `code` renderer below can
   produce: a reply's own `class="code-block"` is stripped by the class hook, and the renderer
   instead emits `cb-<nonce>-<n>`, a class name a reply cannot know (the nonce is drawn once per page
   load and never leaves this module). The hook swaps that marker for the real class and puts the
   fence's own text on the element, so a copy takes the token's text and not whatever the DOM under
   the element happens to hold by then. `fencedTexts` is the per-parse list the markers index into. */
export const CODE_TEXT_ATTR = "data-code-text";
const CODE_NONCE = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
const CODE_MARKER = new RegExp(`^cb-${CODE_NONCE}-(\\d+)$`);
let fencedTexts: string[] = [];

/* marked's own `cleanUrl` (not exported; `marked.esm.js`, used by its default `link` and `image`
   renderers): `encodeURI`, then un-double the `%` it encoded, and `null` for a destination
   `encodeURI` refuses (a lone surrogate). Copied so an image's destination is cleaned exactly as a
   link's is. Re-check against marked's source if marked is upgraded. */
function cleanUrl(href: string): string | null {
  try {
    return encodeURI(href).replace(/%25/g, "%");
  } catch {
    return null;
  }
}

/* Resolve an image link's destination exactly as an ordinary link would, or return `null` when
   marked would emit no link. marked puts the cleaned destination into `href` and lets the HTML
   parser decode entities. Text and attributes decode some entities differently: `&copy=1` stays
   literal in an attribute but changes in text. Decode once through an inert `<template>`, where
   nothing loads or runs, then escape the result for both the attribute and label. `encodeURI`
   escapes quotes and angle brackets in the cleaned destination. */
function resolvedHref(href: string): string | null {
  const cleaned = cleanUrl(href);
  if (cleaned === null) return null;
  const template = document.createElement("template");
  template.innerHTML = `<a href="${cleaned}"></a>`;
  return (template.content.firstElementChild as HTMLAnchorElement).getAttribute("href");
}

marked.use({
  useNewRenderer: true,
  renderer: {
    code({ text, lang }: { text: string; lang?: string }) {
      const language = (lang ?? "").trim().split(/\s+/)[0];
      /* No `highlightAuto`: guessing a language on a fragment produces confidently wrong colours,
         and an unrecognised block as plain monospace is the honest rendering. */
      const body =
        language && hljs.getLanguage(language)
          ? hljs.highlight(text, { language, ignoreIllegals: true }).value
          : escapeHtml(text);
      fencedTexts.push(text);
      return `<pre class="cb-${CODE_NONCE}-${fencedTexts.length - 1}"><code>${markHiddenInCode(body)}</code></pre>`;
    },
    link(token: Tokens.Link) {
      linkDepth += 1;
      try {
        return defaultLink.call(this, token);
      } finally {
        linkDepth -= 1;
      }
    },
    /* Markdown images become plain links showing their alt text and full URL. The sanitizer
       removes `img`, `picture` and `source` so remote images cannot fetch without a click; the
       default image renderer would lose the URL entirely. DOMPurify sanitizes this renderer's
       output like ordinary links, stripping disallowed `href` schemes without a separate
       cleaning step. */
    image({ href, text }: { href: string; title?: string | null; text: string }) {
      /* `text` (the alt) arrives ALREADY html-escaped -- marked's own tokenizer does it at token
         creation (`outputLink`'s image branch, `text: escape$1(text)`), unlike `href` and unlike
         the `code` token's `text` above. Escaping it a second time here visibly broke rendering:
         reproduced in jsdom, `![a & b <c>](url)` rendered the literal characters `a &amp; b &lt;c&gt;`
         on screen (one layer of entity decoding by the HTML parser leaves the second layer as
         literal text) instead of `a & b <c>`. `href` arrives raw, and `resolvedHref` above turns it
         into the URL an ordinary link would go to; that is escaped exactly once, here. */
      const url = resolvedHref(href);
      const shownUrl = url === null ? escapeHtml(href) : escapeHtml(url);
      /* Inside a link: part of that link's own text (see `linkDepth` above) -- the alt text, or,
         absent that, the image URL, so an alt-less badge still says what it was. */
      if (linkDepth > 0) return text.length > 0 ? text : shownUrl;
      const label = text.length > 0 ? `${text}: ${shownUrl}` : shownUrl;
      /* Where marked's own `image`/`link` would emit no element at all (a destination `encodeURI`
         refuses), say what it was without a link to nowhere. */
      if (url === null) return label;
      return `<a href="${shownUrl}">${label}</a>`;
    },
    /* GFM task-list items (`- [ ] x`, `- [x] y`): the default renderer emits a real
       `<input disabled type="checkbox">`, which `FORBID_TAGS` below now drops (`input` is a
       zero-click remote-load vector via `type="image"`) -- silently, since `checkbox()`'s return
       value is spliced into an item's TEXT as a raw string (`listitem()`'s own `checkbox + ' '`),
       never escaped or passed through a tag-aware renderer, so nothing else catches its removal.
       Reproduced in jsdom: before this, `- [ ] todo\n- [x] done` rendered
       `<li> todo</li><li> done</li>` -- indistinguishable, the one piece of information (done or
       not) silently lost. Inert bracket text keeps that information without reintroducing the
       forbidden tag. */
    checkbox({ checked }: Tokens.Checkbox) {
      return checked ? "[x]" : "[ ]";
    },
  },
});

/* Allow only classes emitted by this renderer. A reply must not retain `class="row row-current"`
   or other panel classes, since DOMPurify otherwise preserves arbitrary class values. */
const ALLOWED_CLASS_PREFIXES = ["hljs-", "language-"];
const ALLOWED_CLASSES = new Set(["table-scroll"]);
/* `code-escape` is kept only on a `<span>` whose whole text is an escape (`⟨U+XXXX⟩`), as the code
   renderer writes it, so a reply cannot dress its own text as one. */
const ESCAPE_TEXT = /^⟨U\+[0-9A-F]{4,6}⟩$/;
/* highlight.js's own tiered-scope convention (`core.js`'s `scopeToCSSClass`): a scope name like
   `title.class` becomes TWO space-separated classes on one element, `hljs-title` and a bare
   `class_` (one trailing underscore per nesting depth, so `title.function.invoke` -> `hljs-title`,
   `function_`, `invoke__`) -- collision-avoiding suffixes on JS-reserved words, not `hljs-`
   prefixed. Without this, the allowlist above silently dropped `class_`/`function_`: reproduced in
   jsdom, a Rust `struct Point { x: i32 }` lost `class_` off `Point`'s `<span class="hljs-title
   class_">`, so `index.css`'s `.hljs-title.class_ { color: var(--nv-syn-type); }` stopped matching
   and the type name was coloured as a function instead. `index.css` only ever compounds `class_`
   and `function_` onto `hljs-title`, so this stays that exact, closed set rather than a generic
   "ends in underscore" pattern. A grammar's other modifiers (`invoke__`, `inherited__`,
   `language_`, ...) are stripped here SILENTLY, which is harmless exactly as long as nothing
   styles them: a plain `.hljs-<name>` selector still matches the base class. Nothing else
   notices -- `highlightCoverage.test.ts` deliberately ignores modifiers (see its `hljsClasses`)
   -- so the guard is `markdown.test.ts`'s "every modifier class index.css compounds onto an
   hljs-* selector survives sanitizing", which reads `index.css` itself: a rule written for a new
   modifier without adding it to this set fails there. */
const ALLOWED_HLJS_MODIFIER_CLASSES = new Set(["class_", "function_"]);

/* Installed lazily, on first sanitize, rather than at module load: `dompurify`'s default export is
   a factory that only auto-resolves to a live instance (with `.addHook`) when a real `window` is
   present. `highlightCoverage.test.ts` imports `hljs` from this module under vitest's plain `node`
   environment (no jsdom, no `window`) and never calls `renderMarkdown` -- a module-scope `addHook`
   call broke that import even though nothing there ever sanitizes. */
let sanitizeHooksInstalled = false;
function installSanitizeHooks(): void {
  if (sanitizeHooksInstalled) return;
  sanitizeHooksInstalled = true;
  DOMPurify.addHook("uponSanitizeAttribute", (node, data) => {
    if (data.attrName !== "class") return;
    const allowed = (token: string) =>
      ALLOWED_CLASSES.has(token) ||
      ALLOWED_HLJS_MODIFIER_CLASSES.has(token) ||
      ALLOWED_CLASS_PREFIXES.some((prefix) => token.startsWith(prefix));
    const kept: string[] = [];
    for (const token of data.attrValue.split(/\s+/)) {
      if (token.length === 0) continue;
      const marker = CODE_MARKER.exec(token);
      if (marker !== null) {
        const text = fencedTexts[Number(marker[1])];
        if (text !== undefined && node instanceof Element && node.tagName === "PRE") {
          kept.push("code-block");
          node.setAttribute(CODE_TEXT_ATTR, text);
        }
      } else if (token === "code-escape") {
        if (node instanceof Element && node.tagName === "SPAN" && ESCAPE_TEXT.test(node.textContent ?? "")) {
          kept.push(token);
        }
      } else if (allowed(token)) {
        kept.push(token);
      }
    }
    data.attrValue = kept.join(" ");
    if (data.attrValue === "") data.keepAttr = false;
  });
}

/* One configuration for reply sanitizing.
   - Disable `data-` and `aria-` attributes so reply HTML cannot impersonate panel navigation
     or rendering markers such as `data-nav-stop`, `data-nav-action`, `data-path` and `aria-current`.
   - Forbid `id`, `name`, `role`, `form` and `formaction` separately: the prefix flags do not
     cover them. They can imitate controls or redirect a real form submission.
   - Forbid the `style` tag and attribute. The panel uses inline CSS, so its CSP allows inline
     styles and cannot prevent a reply from painting over controls.
   - Forbid media and embedded content that can load remote resources without a click.
     Include MathML: its namespace permits resource-bearing elements and is not emitted here.
     Markdown images become links in the image renderer before this sanitizer runs.
   - Forbid `background`: DOMPurify allows it on table elements, and WebKitGTK turns it into
     a CSS background image that loads without a click.
   - Forbid `dialog`: its browser styles let an open dialog cover later rows, including
     permission cards, without a style attribute. Keep its contents in the reply's normal flow.
   - Forbid popover and command invoker attributes: their target ids could name panel controls.
     Markdown does not emit these attributes. Forbid `tabindex` so replies cannot add Tab stops. */
/* What the reader sees is what a copy takes. A reply's own HTML can hold text that is in the DOM
   but not seen in three ways this list closes: a `hidden` attribute (a `<span hidden>` tail on a
   command), a collapsed `<details>` body, and a colour of its own -- `<font color>` matching the
   background, or a `bgcolor` cell matching the text. `font` is dropped with its text kept, the
   attributes are dropped, and a `details` body stays, in flow, as ordinary text. The copy paths
   below still take rendered text, not `textContent`, which leaves out what CSS hides. */
const SANITIZE_CONFIG = {
  ALLOW_DATA_ATTR: false,
  ALLOW_ARIA_ATTR: false,
  FORBID_TAGS: [
    "style", "picture", "source", "video", "audio", "input",
    "iframe", "object", "embed", "svg", "image", "img", "math", "dialog", "details", "summary", "font",
    /* Nothing a reply needs, and each can draw something that looks like the panel's own control
       (a button, a field, a meter) or changes the order or the size of what is read: form
       controls, `bdo`/`bdi` (direction override), `marquee`, `canvas`. */
    "button", "form", "textarea", "select", "option", "optgroup", "label", "fieldset", "legend", "output",
    "meter", "progress", "canvas", "marquee", "bdo", "bdi", "datalist", "keygen", "frame", "frameset",
  ],
  FORBID_ATTR: [
    "style", "id", "name", "role", "form", "formaction", "background",
    "popover", "popovertarget", "popovertargetaction", "commandfor", "command", "tabindex", "hidden",
    "color", "bgcolor",
    /* `dir`/`lang` reorder or restyle text, `title` is a tooltip that can say something else than the
       link it sits on, `rowspan`/`colspan`/`width`/`height` can hide a cell, `download` turns a click
       into a file save. `align` stays: marked writes it for a table column's alignment. */
    "dir", "lang", "title", "rowspan", "colspan", "width", "height", "download", "ping", "referrerpolicy",
  ],
  /* DOMPurify's own default pattern with the schemes narrowed to what a reply may link to: `tel:`,
     `sms:`, `ftp:`, `callto:`, `cid:`, `xmpp:` and `matrix:` open other applications. Relative links and
     fragments still pass, as they do by default (the panel's click handler decides what is opened).
     A scheme is a letter followed by letters, digits, `+`, `.` and `-`, so the relative-link branch
     takes a run of all of those and then needs a character that cannot continue a scheme and is not
     its `:`; with digits left out, `h323:x` and `web+foo1:x` read as relative and kept their href. */
  ALLOWED_URI_REGEXP: /^(?:(?:https?|mailto):|[^a-z]|[a-z][a-z0-9+.\-]*(?:[^a-z0-9+.\-:]|$))/i,
};

/** A fenced block is copied byte for byte (`y`), so what it holds must be seen: every character
 *  `revealHidden` knows (bidi controls, zero-width characters, no-break and other spaces, ...) is
 *  drawn as an outlined `⟨U+XXXX⟩` in the block, as it is on a permission card. `html` is
 *  highlight.js's output or escaped text; markup is ASCII there, so only text nodes can match. */
function markHiddenInCode(html: string): string {
  let out = "";
  for (const ch of html) {
    out += isHiddenChar(ch) ? `<span class="code-escape">${escapeText(ch)}</span>` : ch;
  }
  return out;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string,
  );
}

/** Wrap tables so they scroll sideways in their own boxes while the list stays fixed horizontally.
 *  Wrap before sanitizing so DOMPurify checks and keeps the wrapper. */
function wrapTables(html: string): string {
  return html.replace(/<table>/g, '<div class="table-scroll"><table>').replace(/<\/table>/g, "</table></div>");
}

let parses = 0;
/** How many times `marked.parse` has run in this page (V3's measurement). */
export function markdownParseCount(): number {
  return parses;
}

/**
 * Convert Markdown to HTML, highlight it, then sanitize the complete output.
 * Highlighting emits spans around escaped text; DOMPurify must check that generated HTML too.
 * Sanitizing first would leave the highlighter's later output unchecked.
 * Keep the whole pipeline in one exported function so components cannot skip a step.
 *
 * A bounded cache evicts in insertion order to avoid parsing every visible message on each j press.
 * `perf.test.tsx` measured 500 parses per press over 500 messages without caching.
 * Theme changes use CSS variables rather than changing this HTML, so cached strings stay valid.
 */
export const MARKDOWN_CACHE_LIMIT = 1000;
const cache = new Map<string, string>();

export function renderMarkdown(text: string): string {
  const hit = cache.get(text);
  if (hit !== undefined) return hit;
  parses += 1;
  installSanitizeHooks();
  fencedTexts = [];
  let html: string;
  try {
    html = DOMPurify.sanitize(wrapTables(marked.parse(text) as string), SANITIZE_CONFIG);
  } finally {
    fencedTexts = [];
  }
  if (cache.size >= MARKDOWN_CACHE_LIMIT) cache.delete(cache.keys().next().value as string);
  cache.set(text, html);
  return html;
}

/** Drops `text`'s rendered HTML from the cache: a reply that kept streaming has no further use for
 *  the HTML of what it said before. Only a cost if another row shows the very same text (it is
 *  rendered again the next time that row mounts). */
export function forgetMarkdown(text: string): void {
  cache.delete(text);
}

/** Whether `text`'s rendered HTML is in the cache (the performance tests read it). */
export function isMarkdownCached(text: string): boolean {
  return cache.has(text);
}

/** The text a reader sees in `el`: `innerText` skips whatever the page does not render (`hidden`,
 *  `display: none`, a collapsed `<details>`), `textContent` does not. `innerText` is absent only
 *  where nothing is laid out (jsdom), where the two coincide for the markup the sanitizer allows. */
export function renderedText(el: HTMLElement): string {
  return el.innerText ?? el.textContent ?? "";
}

/** The text of a fenced code block a copy should take. A block the renderer wrote carries its own
 *  fence's text (`CODE_TEXT_ATTR`), which no later markup inside the element can change; only a block
 *  without it (not one this module produced) falls back to what is rendered. */
export function codeBlockText(block: HTMLElement): string {
  const own = block.getAttribute(CODE_TEXT_ATTR);
  if (own !== null) return own;
  const code = block.querySelector<HTMLElement>("code");
  return renderedText(code ?? block);
}
