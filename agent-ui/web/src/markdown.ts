import { marked } from "marked";
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
      return `<pre class="code-block"><code>${body}</code></pre>`;
    },
  },
});

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string,
  );
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
 */
export function renderMarkdown(text: string): string {
  return DOMPurify.sanitize(marked.parse(text) as string);
}
