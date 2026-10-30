import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import css from "highlight.js/lib/languages/css";
import diff from "highlight.js/lib/languages/diff";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import go from "highlight.js/lib/languages/go";
import ini from "highlight.js/lib/languages/ini";
import java from "highlight.js/lib/languages/java";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import kotlin from "highlight.js/lib/languages/kotlin";
import lua from "highlight.js/lib/languages/lua";
import makefile from "highlight.js/lib/languages/makefile";
import markdown from "highlight.js/lib/languages/markdown";
import php from "highlight.js/lib/languages/php";
import plaintext from "highlight.js/lib/languages/plaintext";
import python from "highlight.js/lib/languages/python";
import ruby from "highlight.js/lib/languages/ruby";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";
import { NONCE_ATTR } from "./env";

/* The languages READMEs and notes are written in, registered from `lib/core` rather than the default bundle of
   about 190, and never guessed: a fence without a known language is plain text. The set adds at most five times
   highlight.js's own core to the page (about 88 KB minified over a 21 KB core);
   `highlight.size.test.ts` holds that, so a grammar added later is weighed against the same budget. */
const GRAMMARS = [
  ["bash", bash], ["c", c], ["cpp", cpp], ["csharp", csharp], ["css", css], ["diff", diff],
  ["dockerfile", dockerfile], ["go", go], ["ini", ini], ["java", java], ["javascript", javascript], ["json", json],
  ["kotlin", kotlin], ["lua", lua], ["makefile", makefile], ["markdown", markdown], ["php", php],
  ["plaintext", plaintext], ["python", python], ["ruby", ruby], ["rust", rust], ["sql", sql],
  ["typescript", typescript], ["xml", xml], ["yaml", yaml],
] as const;

/* Fence names in common use that no grammar lists as its own alias. */
const EXTRA_ALIASES: Record<string, string> = { shell: "bash" };

for (const [name, language] of GRAMMARS) hljs.registerLanguage(name, language);
for (const [alias, languageName] of Object.entries(EXTRA_ALIASES)) hljs.registerAliases([alias], { languageName });

/** Every name a fence may give: each grammar's name and aliases, sorted, once each. */
export const LANGUAGES: readonly string[] = Object.freeze(
  [
    ...new Set([
      ...GRAMMARS.flatMap(([name]) => [name, ...(hljs.getLanguage(name)?.aliases ?? [])]),
      ...Object.keys(EXTRA_ALIASES),
    ]),
  ].sort(),
);

/** highlight.js's HTML for `code`, or null when `lang` names no registered grammar. Every element it opens carries
 *  the render's nonce, which is what lets its `class` attributes through the sanitiser: a document's own HTML
 *  cannot know the nonce, so it cannot dress its text as highlighted code. */
export function highlightCode(code: string, lang: string, nonce: string): string | null {
  const name = lang.trim().toLowerCase();
  if (name === "" || !hljs.getLanguage(name)) return null;
  const html = hljs.highlight(code, { language: name, ignoreIllegals: true }).value;
  return html.split("<span ").join(`<span ${NONCE_ATTR}="${nonce}" `);
}
