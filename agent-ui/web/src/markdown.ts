import { fenced } from "./paths";
import { marked, Renderer, type Token, type Tokens } from "marked";
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

/* The eleven languages the panel highlights, registered from `lib/core` rather than the default bundle
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
   internally (the class-to-CSS coverage check), rather than a second copy of the registration
   block above drifting out of sync with this one. */
export { hljs };

/* marked 13 types `code` with an object token, but its default compatibility shim passes
   positional strings, leaving destructured `text` undefined and fences empty. `useNewRenderer: true`
   selects the token form. Re-check the flag and token shape when upgrading marked. */
/* An image inside a link's text (the badge pattern
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
export const CODE_LANGUAGE_ATTR = "data-code-language";
const CODE_NONCE = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
const CODE_MARKER = new RegExp(`^cb-${CODE_NONCE}-(\\d+)$`);
let fencedTexts: { text: string; lang: string | null; diagram: boolean }[] = [];

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
   itself, because an image's link also SHOWS the URL as text, and text is decoded by different
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
    code(token: Tokens.Code) {
      const { text, lang, raw } = token;
      const language = (lang ?? "").trim().split(/\s+/)[0];
      /* No `highlightAuto`: guessing a language on a fragment produces confidently wrong colours,
         and an unrecognised block as plain monospace is the honest rendering. */
      const body =
        language && hljs.getLanguage(language)
          ? hljs.highlight(text, { language, ignoreIllegals: true }).value
          : escapeHtml(text);
      fencedTexts.push({ text, lang: language || null, diagram: language === "mermaid" && fenceClosed(raw, drawingEnded || token !== lastDrawnToken || drawingEndsInNewline) });
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
    /* A markdown image is never loaded. `img`/`picture`/`source` are
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
         the `code` token's `text` above. Escaping it twice makes `![a & b <c>](url)` show the literal
         `a &amp; b &lt;c&gt;` (one layer of HTML entity decoding leaves the second as literal text)
         instead of `a & b <c>`. `href` arrives raw, and `resolvedHref` above turns it into the URL
         an ordinary link would go to; that is escaped exactly once, here. */
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
    /* GFM checkboxes must use inert bracket text: the default renderer inserts an `<input>` as raw
       item text, which the sanitizer drops because `type="image"` can load remotely without a click.
       The drop is silent -- the return value is spliced into the item's text unescaped and no
       tag-aware renderer sees it, so nothing else catches its removal -- and it makes `- [ ] todo` and
       `- [x] done` indistinguishable. Bracket text keeps that one piece of information without the
       forbidden tag. */
    checkbox({ checked }: Tokens.Checkbox) {
      return checked ? "[x]" : "[ ]";
    },
  },
});

/* Classes markdown.ts itself ever emits. A reply's own
   `class="row row-current"` (a reply drawn as a fake panel row) or anything else copied from `index.css`
   must not survive sanitizing -- DOMPurify's default `class` handling keeps any value verbatim. */
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
        const block = fencedTexts[Number(marker[1])];
        if (block !== undefined && node instanceof Element && node.tagName === "PRE") {
          kept.push("code-block");
          node.setAttribute(CODE_TEXT_ATTR, JSON.stringify(block.text));
          node.setAttribute(CODE_LANGUAGE_ATTR, JSON.stringify(block.lang));
          if (block.diagram) node.setAttribute("data-code-diagram", "1");
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

/* A reply must not fake panel controls, cover real rows or load remote resources without a click.
   - Disable data/ARIA attributes so reply HTML cannot borrow navigation and rendering markers.
     Also forbid id/name/role/form/formaction: these lack those prefixes, can fake controls or redirect submits.
   - Forbid style tags and attributes: inline panel CSS requires CSP's style allowance, so CSP cannot
     prevent a reply from painting an overlay.
   - Forbid media, image, frame, object, embed, SVG, MathML and input tags as zero-click load vectors.
     DOMPurify's default profile allows the whole `math` namespace, including `<mglyph src=...>`;
     that is not verified as a WebKitGTK load, but has the shape of `svg image href` and nothing here
     emits `<math>`. Markdown images become links before sanitization.
   - Forbid background: DOMPurify allows it on table elements, including https URLs; WebKitGTK turns
     it into a background-image presentational hint that loads without a click.
   - Forbid dialog: its UA styling positions an open dialog absolutely and paints it opaque over later
     rows, including permission cards. The unpositioned `.row` and `.message-list` let it position
     against `.agent-ui-scroller`. Keep its content in flow instead.
   - Forbid popover/invoker/command attributes: each needs a click, an id or a Tab to do anything and
     markdown emits none of them, but the invokers name a target by id, which could be one of the
     panel's own ids since only the reply's own `id`s are forbidden. Forbid tabindex so replies
     cannot join the controls' Tab stops. */
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

/** Sanitized code may contain entity-decoded characters or nested markup. Replace only text nodes,
 *  using fixed spans and text children, so revealing a character cannot introduce active HTML. Text nodes are read
 *  after the HTML parser decoded every character reference, so `&#x202e;` and `&rlm;` count as what they become.
 *  `everywhere` also reveals prose (`proseHiddenAt`). Returns how many characters it found; past `cap` it changes
 *  nothing. */
function revealCodeText(fragment: DocumentFragment, everywhere = false, cap = Infinity): number {
  const walker = document.createTreeWalker(fragment, NodeFilter.SHOW_TEXT);
  const found: { node: Text; chars: string[]; at: number[] }[] = [];
  let count = 0;
  while (walker.nextNode()) {
    const node = walker.currentNode as Text;
    const inCode = node.parentElement?.closest("code, pre, kbd, samp, tt") != null;
    if (!inCode && !everywhere) continue;
    const chars = [...node.data];
    const at = inCode ? codeHiddenAt(chars) : proseHiddenAt(chars);
    if (at.length === 0) continue;
    found.push({ node, chars, at });
    count += at.length;
  }
  if (count > cap) return count;
  for (const { node, chars, at } of found) {
    const parts = document.createDocumentFragment();
    let from = 0;
    for (const i of at) {
      parts.append(document.createTextNode(chars.slice(from, i).join("")));
      const span = document.createElement("span");
      span.className = "code-escape";
      span.textContent = escapeText(chars[i]);
      parts.append(span);
      from = i + 1;
    }
    parts.append(document.createTextNode(chars.slice(from).join("")));
    node.replaceWith(parts);
  }
  return count;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string,
  );
}

/** A table scrolls sideways in its own box; the list never does. Wrapped BEFORE the
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
 * Highlighting BEFORE the sanitize is the safe order: highlight.js
 * emits `<span class="hljs-*">` around text it has already escaped, and the whole document still
 * passes through DOMPurify afterwards, so the existing security boundary does not move. Sanitizing
 * first and highlighting after would hand highlight.js sanitized HTML to re-parse and put its
 * output on the page unchecked.
 *
 * One function, exported from one module, so no component can do half of this.
 *
 * Memoized in a cache bounded by size and evicted in insertion order. A `j` press re-renders the
 * whole panel; before this, every visible message was re-parsed (marked + highlight.js +
 * DOMPurify) on every press -- measured at 500 `marked.parse` calls per press over 500 messages
 * (`perf.test.tsx`). The theme never changes this HTML (highlighting emits classes, colours come
 * from CSS variables), so a cached string cannot go stale.
 */
export const MARKDOWN_CACHE_CHARS = 64 * 1024 * 1024;
let cacheCharsLimit = MARKDOWN_CACHE_CHARS;
const cache = new Map<string, string>();
/** The characters the cache holds, keys and values: bounded by size, not by entries, since a tab switch mounts every
 *  reply of the conversation and a pass over more entries than the cache holds evicts each one just before it is
 *  needed, so a long conversation's every switch would parse every reply again. */
let cacheChars = 0;

/** Sets the cache's bound and returns the one it replaces (the tests use a small one). */
export function setMarkdownCacheChars(chars: number): number {
  const before = cacheCharsLimit;
  cacheCharsLimit = chars;
  evictDownTo(chars);
  return before;
}

function evictDownTo(chars: number): void {
  while (cacheChars > chars) forgetMarkdown(cache.keys().next().value as string);
}

let drawingEnded = true;
let lastDrawnToken: Token | null = null;
let drawingEndsInNewline = false;

/** A closing line still being written may gain more characters; only a newline or stream end settles it. */
export function fenceClosed(raw: string, ended: boolean): boolean {
  const opener = /^ {0,3}(`{3,}|~{3,})[^\n]*\n/.exec(raw);
  if (!opener) return false;
  const close = new RegExp(`(?:^|\\n) {0,3}${opener[1][0]}{${opener[1].length},}[ \t]*(\\n[ \t\\n]*)?$`).exec(raw.slice(opener[0].length));
  return close !== null && (ended || close[1] !== undefined);
}

function draw(text: string, ended = true): string {
  parses += 1;
  installSanitizeHooks();
  fencedTexts = [];
  drawingEnded = ended;
  try {
    const tokens = marked.lexer(text);
    lastDrawnToken = null;
    drawingEndsInNewline = text.endsWith("\n");
    // Marked leaves a separator newline to the following token. A non-final block's closing line is settled too.
    marked.walkTokens(tokens, (token) => { if (token.type !== "space") lastDrawnToken = token; });
    const html = wrapTables(marked.parser(tokens));
    if (!/<(code|pre|kbd|samp|tt)\b/i.test(html)) return DOMPurify.sanitize(html, SANITIZE_CONFIG);
    const fragment = DOMPurify.sanitize(html, {
      ...SANITIZE_CONFIG, RETURN_DOM_FRAGMENT: true,
    });
    revealCodeText(fragment);
    const container = document.createElement("div");
    container.append(fragment);
    return container.innerHTML;
  } finally {
    fencedTexts = [];
  }
}

export function renderMarkdown(text: string): string {
  const hit = cache.get(text);
  if (hit !== undefined) return hit;
  const html = draw(text);
  const chars = text.length + html.length;
  if (chars <= cacheCharsLimit) {
    evictDownTo(cacheCharsLimit - chars);
    cache.set(text, html);
    cacheChars += chars;
  }
  return html;
}

/** In prose, every character `revealHidden` escapes (controls, format and bidi characters, every default-ignorable
 *  code point, the blank-rendering fillers and variation selectors) except two kinds that are how text is written,
 *  not a disguise: the spaces other than the plain one (`\p{Zs}`: a no-break space draws as a space), and the parts
 *  of an emoji (`proseHiddenAt`). Code keeps the full set (`isHiddenChar`). */
const SPACE = /\p{Zs}/u;
/** What a presentation selector (U+FE0E text, U+FE0F emoji) may follow and stay: a character with an emoji form. */
const EMOJI = /\p{Emoji}/u;
/** What may come right before a joiner of an emoji ZWJ sequence: an emoji, its presentation selector or its skin tone. */
const BEFORE_JOINER = /[\p{Extended_Pictographic}\uFE0F\u{1F3FB}-\u{1F3FF}]/u;
const PICTOGRAPH = /\p{Extended_Pictographic}/u;
/** The tag characters that stay unescaped: the tag specs and CANCEL TAG of the recommended subdivision flags, each
 *  after U+1F3F4 BLACK FLAG. An emoji tag sequence (UTS #51 ED-14a) is a base, one or more tag characters from
 *  U+E0020 to U+E007E and U+E007F CANCEL TAG; the only ones Unicode defines as valid are flags of a subdivision
 *  (U+1F3F4, a CLDR subdivision id spelled in tag lowercase letters and digits, CANCEL TAG), and of those only
 *  England, Scotland and Wales are recommended for interchange. Any other spelling, even a valid subdivision such
 *  as `usca`, draws as a plain black flag in most fonts while its tags carry up to seven letters no one sees, so
 *  it is revealed: a run after another emoji, a run with no CANCEL TAG, a longer run, tags that are not lowercase
 *  letters or digits. */
const FLAG_TAGS = ["gbeng", "gbsct", "gbwls"].map((code) => [
  ...[...code].map((c) => String.fromCodePoint(0xe0000 + c.charCodeAt(0))),
  "\u{E007F}",
]);

/** The indexes in `chars` of the characters code reveals: all that `revealHidden` escapes. */
function codeHiddenAt(chars: string[]): number[] {
  const at: number[] = [];
  for (let i = 0; i < chars.length; i++) if (isHiddenChar(chars[i])) at.push(i);
  return at;
}

/** The indexes in `chars` (one text node's code points) of the characters prose reveals, in one pass. A joiner
 *  stays only between an emoji and the emoji it joins, a presentation selector only right after a character with an
 *  emoji form, a tag character only inside a recommended flag. */
function proseHiddenAt(chars: string[]): number[] {
  const at: number[] = [];
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i];
    if (ch === "\u{1F3F4}") {
      const tags = FLAG_TAGS.find((flag) => flag.every((tag, k) => chars[i + 1 + k] === tag));
      if (tags !== undefined) i += tags.length;
      continue;
    }
    if (!isHiddenChar(ch) || SPACE.test(ch)) continue;
    if (ch === "\u200D" && i > 0 && BEFORE_JOINER.test(chars[i - 1]) && PICTOGRAPH.test(chars[i + 1] ?? "")) continue;
    if ((ch === "\uFE0E" || ch === "\uFE0F") && i > 0 && EMOJI.test(chars[i - 1])) continue;
    at.push(i);
  }
  return at;
}

/** At most this many characters are drawn as escapes in a plan's prose; past it the plan is not drawn as
 *  Markdown at all (`renderMarkdownRevealed` returns `null`) and its reader shows it as escaped plain text. */
export const MAX_PROSE_ESCAPES = 4_000;
const revealed = new Map<string, string | null>();

/** `renderMarkdown(text)` with the characters that disguise prose (`proseHiddenAt`) drawn as outlined escapes outside
 *  code too: for text a person approves (Claude's plan), where a bidi control or a zero-width character in a sentence
 *  could make it read as something it does not say. The sanitized HTML is parsed into an inert template and only
 *  its text nodes change, with fixed spans and text children, so revealing a character cannot introduce active
 *  HTML. They are found in the parsed text, not in `text`: markdown leaves a character reference (`&#x202e;`, `&rlm;`)
 *  for the HTML parser to decode, so a plan can hold none of them literally and still draw them. `null` when the
 *  page would hold more than `MAX_PROSE_ESCAPES` escapes, counting the ones code already drew (`draw` reveals code
 *  before this sees it). Kept for the last few plans, so a render of the dock does not draw it again. */
export function renderMarkdownRevealed(text: string): string | null {
  const held = revealed.get(text);
  if (held !== undefined) return held;
  let html: string | null = renderMarkdown(text);
  const template = document.createElement("template");
  template.innerHTML = html;
  template.content.querySelectorAll("[data-code-diagram]").forEach((el) => el.removeAttribute("data-code-diagram"));
  const drawn = template.content.querySelectorAll(".code-escape").length;
  const count = revealCodeText(template.content, true, MAX_PROSE_ESCAPES - drawn);
  if (drawn + count > MAX_PROSE_ESCAPES) html = null;
  else html = template.innerHTML;
  if (revealed.size >= 8) revealed.delete(revealed.keys().next().value as string);
  revealed.set(text, html);
  return html;
}

/** Drops `text`'s rendered HTML from the cache: a reply that kept streaming has no further use for
 *  the HTML of what it said before. Only a cost if another row shows the very same text (it is
 *  rendered again the next time that row mounts). */
export function forgetMarkdown(text: string): void {
  const html = cache.get(text);
  if (html === undefined) return;
  cache.delete(text);
  cacheChars -= text.length + html.length;
}

/** A reply being written, drawn up to `text`: `added` is the HTML of the blocks this step completed, to go after those
 *  of the steps before it, and `tail` the HTML of the open end, which the next step replaces. Together they are, to the
 *  character, what `renderMarkdown(text)` gives. `restarted`: nothing from an earlier step stands. */
export type StreamedMarkdown = {
  text: string;
  /** How much of `text` is in the completed blocks. */
  settled: number;
  /** Never split: `text` holds what makes the blocks depend on each other (`settledEnd`). */
  whole: boolean;
  added: string;
  tail: string;
  restarted: boolean;
};

function holdsRawHtml(token: Token): boolean {
  let found = false;
  marked.walkTokens([token], (inner) => {
    if (inner.type === "html") found = true;
  });
  return found;
}

/** Where the completed blocks of `text` end, or `null` when the text can never be split.
 *
 *  The block lexer only moves forward, and past a blank line it has nothing left of the lines before it: no paragraph
 *  it could join, no setext underline reaching back over them (such an underline claims every line up to a blank one,
 *  a thematic break included). So what lies past a blank line draws the same with or without what came before it,
 *  unless the two are tied some other way:
 *  - a link reference definition reaches every block, the ones before it too: such a text is never split;
 *  - raw HTML can leave an element open across later blocks: nothing from its block on is complete, and a piece of HTML
 *    never starts at it either, since a document's first tags are parsed as its head, which changes the space after them.
 *
 *  The end is a blank line whose next line has ended. A line still being written can turn out to continue the block
 *  before the blank line: `2` becoming `2. ` after a list's first item. Counted in lines, because the lexer rewrites
 *  tabs and blank lines before it cuts tokens, so its `raw` strings do not add up to `text`. Two things change the line
 *  count: a carriage return, and an indented line the lexer folds into the paragraph above it, which its `raw` joins
 *  with one newline too many. A text with a carriage return is not split, and nothing past such a paragraph, which
 *  is the only paragraph whose `raw` can hold a blank line, is complete. */
function settledEnd(text: string): number | null {
  if (text.includes("\r")) return null;
  const tokens = marked.lexer(text);
  if (Object.keys(tokens.links).length > 0) return null;
  let line = 0;
  const ends: number[] = [];
  for (let i = 0; i < tokens.length && !holdsRawHtml(tokens[i]); i++) {
    const { raw, type } = tokens[i];
    if ((type === "paragraph" || type === "text") && raw.trimEnd().includes("\n\n")) break;
    for (let at = raw.indexOf("\n"); at !== -1; at = raw.indexOf("\n", at + 1)) line += 1;
    const next = tokens[i + 1];
    if (raw.endsWith("\n\n") && next !== undefined && !holdsRawHtml(next)) ends.push(line);
  }
  const offsetOf = (wanted: number) => {
    let at = 0;
    for (let seen = 0; seen < wanted; seen++) at = text.indexOf("\n", at) + 1;
    return at;
  };
  for (const end of ends.slice(-2).reverse()) {
    const offset = offsetOf(end);
    if (text.indexOf("\n", offset) !== -1) return offset;
  }
  return 0;
}

/** Draws `text`, which goes on from `held` when `held`'s text is the start of it, parsing only what comes after the
 *  blocks `held` completed. Anything else starts over. `held` itself comes back when the text has not changed. Every
 *  piece of HTML goes through `draw`, the one pipeline `renderMarkdown` uses. */
export function streamMarkdown(held: StreamedMarkdown | null, text: string): StreamedMarkdown {
  if (held !== null && held.text === text) return held;
  const goesOn = held !== null && !held.whole && text.startsWith(held.text);
  const from = goesOn ? held.settled : 0;
  const open = text.slice(from);
  const end = held !== null && held.whole && text.startsWith(held.text) ? null : settledEnd(open);
  if (end === null) return { text, settled: 0, whole: true, added: "", tail: draw(text, false), restarted: true };
  return {
    text,
    settled: from + end,
    whole: false,
    added: end === 0 ? "" : draw(open.slice(0, end), false),
    tail: draw(open.slice(end), false),
    restarted: !goesOn,
  };
}

/** Whether `text`'s rendered HTML is in the cache (the performance tests read it). */
export function isMarkdownCached(text: string): boolean {
  return cache.has(text);
}

/** The text a reader sees in `el`: `innerText` skips whatever the page does not render (`hidden`,
 *  `display: none`, a collapsed `<details>`), `textContent` does not. `innerText` is absent only
 *  where nothing is laid out (jsdom), where the two coincide for the markup the sanitizer allows. */
export function renderedText(el: HTMLElement): string {
  if (el.matches(".diagram-drawn, .diagram-error") || el.querySelector(".diagram-drawn, .diagram-error")) {
    const copy = el.cloneNode(true) as HTMLElement;
    copy.querySelectorAll(".diagram-error").forEach((note) => note.remove());
    copy.querySelectorAll<HTMLElement>("pre.diagram-drawn").forEach((block) => {
      const source = document.createElement("pre");
      source.style.whiteSpace = "pre";
      source.textContent = fenced(codeBlockText(block), "mermaid");
      block.replaceWith(source);
    });
    // Read in the document so innerText preserves the row's normal line breaks and hidden text rules.
    copy.style.position = "absolute";
    copy.style.left = "-100000px";
    copy.style.opacity = "0";
    document.body.append(copy);
    const text = copy.innerText ?? copy.textContent ?? "";
    copy.remove();
    return text;
  }
  return el.innerText ?? el.textContent ?? "";
}

/** The text of a fenced code block a copy should take. A block the renderer wrote carries its own
 *  fence's text (`CODE_TEXT_ATTR`), which no later markup inside the element can change; only a block
 *  without valid stored text falls back to what is rendered. */
export function codeBlockText(block: HTMLElement): string {
  const own = block.getAttribute(CODE_TEXT_ATTR);
  if (own !== null) {
    try {
      const text: unknown = JSON.parse(own);
      if (typeof text === "string") return text;
    } catch {
      // A damaged attribute leaves the rendered code as the copy source.
    }
  }
  const code = block.querySelector<HTMLElement>("code");
  return renderedText(code ?? block);
}

/** The language recorded with this rendered fence, independent of other source tokens. */
export function codeBlockLanguage(block: HTMLElement): string | null {
  try {
    const lang: unknown = JSON.parse(block.getAttribute(CODE_LANGUAGE_ATTR) ?? "null");
    return typeof lang === "string" ? lang : null;
  } catch {
    return null;
  }
}
