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

/* The twelve languages spec §3.3 names, registered from `lib/core` rather than the default bundle
   so the single-file `dist` carries these and not all 190. `tsx` is typescript's own alias, and
   `toml` is highlight.js's `ini` grammar, which is what it ships for TOML. */
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
   internally (spec §7's class-to-CSS coverage check), rather than a second copy of the registration
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
/* v1-hardening Task 1 fix rounds 1-2: an image inside a link's text (the badge pattern
   `[![badge](img-url)](real-url)`, or mixed content like `[see ![b](img)](real-url)`) must not
   become the `image` renderer's own `<a>` (below) nested inside the `link` renderer's `<a>`. Left
   as two overlapping anchors, an HTML parser splits them into SIBLINGS (the adoption agency
   algorithm never lets an `<a>` nest inside another) -- reproduced in jsdom: the real link loses
   the image's part of its text, and that part, visible and clickable, points at the image URL
   instead. `linkDepth` is how `image` knows it is inside a link: `marked.parse` is synchronous and
   links cannot nest (CommonMark), so a module counter the `link` renderer raises around its own
   inline parse is exact, at any depth of emphasis in between. `defaultLink` is saved before
   `marked.use()` overrides `link`, so `link` below can still call the REAL default implementation
   (its href cleaning and its `title` handling stay marked's own) -- `Renderer.prototype.link` is
   already written in the object-argument ("new renderer") shape `marked.d.ts` documents, so no
   positional-args shim applies to it, unlike `code` below. */
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

/* The URL a click on an image's link goes to, as an ordinary link to the same destination would
   have it -- or `null` where marked's own renderers would emit no link at all. marked's `link`
   emits the cleaned destination RAW into `href="..."` and lets the HTML parser decode entity
   references in it, which is what gives CommonMark's semantics (`&amp;` in a destination is `&` in
   the URL; `?a=1&amp;b=2` has a parameter `b`, not `amp;b`). This module needs the decoded string
   itself, because R1's image link also SHOWS the URL as text, and text is decoded by different
   rules than an attribute: a legacy reference with no `;` (`&copy=1`) is left alone in an
   attribute and decoded to a copyright sign in text, so emitting the raw string in both places
   would show one URL and go to another. So the decoding is done once, here, by the same HTML parser an
   ordinary link's `href` goes through (an inert `<template>`: nothing in it loads or runs; the
   cleaned string holds no `"`, `<` or `>`, which `encodeURI` escapes), and the result is then
   escaped once for both the attribute and the label. */
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
      return `<pre class="cb-${CODE_NONCE}-${fencedTexts.length - 1}"><code>${body}</code></pre>`;
    },
    link(token: Tokens.Link) {
      linkDepth += 1;
      try {
        return defaultLink.call(this, token);
      } finally {
        linkDepth -= 1;
      }
    },
    /* R1 / panel-content finding 3: a markdown image is never loaded. `img`/`picture`/`source` are
       dropped entirely by the sanitizer below (a remote pixel must not fetch with no click), so an
       `![alt](url)` that reached the DEFAULT image renderer would just vanish -- losing the one
       piece of information (the URL) a reader needs to judge whether to open it. This renderer
       turns it into a plain link instead, showing the alt text and the full URL; DOMPurify still
       runs on the result afterward (same order as `link` above and every other renderer here), so
       an `href` scheme it doesn't allow (`javascript:`, …) is stripped exactly as it is for a
       normal markdown link -- there is no separate cleaning step to keep in sync. */
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

/* R1 / panel-content findings 1-3: classes markdown.ts itself ever emits. A reply's own
   `class="row row-current"` (finding 1's adjacent probe) or anything else copied from `index.css`
   must not survive sanitizing -- DOMPurify's default `class` handling keeps any value verbatim. */
const ALLOWED_CLASS_PREFIXES = ["hljs-", "language-"];
const ALLOWED_CLASSES = new Set(["table-scroll"]);
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
      } else if (allowed(token)) {
        kept.push(token);
      }
    }
    data.attrValue = kept.join(" ");
    if (data.attrValue === "") data.keepAttr = false;
  });
}

/* R1 / panel-content findings 1-3, one config for the one `sanitize` call below.
   - `ALLOW_DATA_ATTR: false` / `ALLOW_ARIA_ATTR: false`: a reply cannot carry `data-nav-stop`,
     `data-nav-action`, `data-path`, `aria-current`, … -- the attributes the panel's own navigation
     and rendering key off.
   - `id`/`name`/`role`/`form`/`formaction`: not `data-`/`aria-` prefixed, so the two flags above
     don't reach them; `role` alone is enough to fake a `role="button"` node, and `formaction` can
     redirect a real `<form>` submit.
   - `style` tag and attribute: forbidden outright (finding 2) -- a CSP cannot do this job, because
     the panel's own CSS is inline, so `style-src` must already allow it.
   - `picture`/`source`/`video`/`audio`/`input`/`iframe`/`object`/`embed`/`svg`/`image`/`img`/`math`:
     every zero-click remote-load vector finding 3 reproduced, plus MathML (DOMPurify's default
     profile allows the whole `math` namespace, including `<mglyph src=…>`; not itself reproduced as
     a WebKitGTK load, but the same shape as the `svg image href` vector this list already closes,
     and nothing here ever legitimately emits `<math>`). Markdown's own `![alt](url)` never
     reaches this list -- the `image` renderer above turns it into a link before marked's output
     ever gets here.
   - `background`: the one FORBID_ATTR gap a review found after the rest of this list -- DOMPurify's
     default allowlist keeps `background` on `table`/`tr`/`td`/`th`/`thead`/`tbody`/`tfoot` (an
     https value passes its URI check too), and WebKitGTK maps it to a `background-image`
     presentational hint, so it loads with no click, the same class of vector as finding 3.
     Reproduced in jsdom: `<table background="https://…"><tr><td>x</td></tr></table>` and a `<td
     background=…>` inside a real markdown table both kept the attribute unchanged before this.
   - `dialog` (fix round 2): an overlay with no `style` at all. WebKitGTK's own UA stylesheet gives
     it `position: absolute; inset-inline: 0; margin: auto; background-color: Canvas` and
     `dialog[open] { display: block }`; `.row` and `.message-list` are not positioned, so an
     `<dialog open>` in a reply lays out against `.agent-ui-scroller` and paints, opaque, over the
     rows after it -- the real permission card included, which is finding 2's overlay rebuilt from
     an allowed tag. Reproduced in jsdom: it came through unchanged, `open` and all. Its content
     is kept, in flow, as the rest of the reply.
   - `popover`/`popovertarget`/`popovertargetaction`/`commandfor`/`command`/`tabindex` (fix round
     2): each needs a click, an id or a Tab to do anything, but none is anything markdown emits,
     and the invoker attributes name their target by id -- which could be one of the PANEL's own
     ids, since only the reply's own `id`s are forbidden above. `tabindex` would make part of a
     reply a stop for `Tab`, which is how the panel's own controls are reached. */
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
  ],
  FORBID_ATTR: [
    "style", "id", "name", "role", "form", "formaction", "background",
    "popover", "popovertarget", "popovertargetaction", "commandfor", "command", "tabindex", "hidden",
    "color", "bgcolor",
  ],
};

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string,
  );
}

/** T1: a table scrolls sideways in its own box; the list never does (`eb22380`). Wrapped BEFORE the
 *  sanitize, so DOMPurify still sees (and keeps) the wrapper. */
function wrapTables(html: string): string {
  return html.replace(/<table>/g, '<div class="table-scroll"><table>').replace(/<\/table>/g, "</table></div>");
}

let parses = 0;
/** How many times `marked.parse` has run in this page (V3's measurement). */
export function markdownParseCount(): number {
  return parses;
}

/**
 * Markdown to HTML, highlighted and then sanitized — in that order, deliberately.
 *
 * Highlighting BEFORE the sanitize is what spec §3.3 asks for and is the safe order: highlight.js
 * emits `<span class="hljs-*">` around text it has already escaped, and the whole document still
 * passes through DOMPurify afterwards, so the existing security boundary does not move. Sanitizing
 * first and highlighting after would hand highlight.js sanitized HTML to re-parse and put its
 * output on the page unchecked.
 *
 * One function, exported from one module, so no component can do half of this.
 *
 * V3 (ruling 33): memoized in a bounded, insertion-order-evicted cache. A `j` press re-renders the
 * whole panel; before this, every visible message was re-parsed (marked + highlight.js +
 * DOMPurify) on every press -- measured at 500 `marked.parse` calls per press over 500 messages
 * (`perf.test.tsx`). The theme never changes this HTML (highlighting emits classes, colours come
 * from CSS variables), so a cached string cannot go stale.
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
