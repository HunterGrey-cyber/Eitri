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
 * Convert Markdown to HTML, highlight it, then sanitize the complete output.
 * Highlighting emits spans around escaped text; DOMPurify must check that generated HTML too.
 * Sanitizing first would leave the highlighter's later output unchecked.
 * Keep the whole pipeline in one exported function so components cannot skip a step.
 */
export function renderMarkdown(text: string): string {
  return DOMPurify.sanitize(marked.parse(text) as string);
}
