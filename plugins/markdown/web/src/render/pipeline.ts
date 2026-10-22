import type { DocPrefixes, PreviewSettings, TextBody } from "../protocol";
import { trace } from "../trace";
import type { Engine } from "./engine";
import { newNonce, type RenderEnv } from "./env";
import { fillMath } from "./math";
import type { MermaidRunner } from "./mermaid";
import { sanitizeDocument } from "./sanitize";

/** One rendered snapshot: the buffer and change tick it came from, the source lines it was rendered from (what a
 *  yank copies), the sanitised fragment ready to go into the page, and what it held. */
export interface RenderResult {
  buf: number;
  tick: number;
  name: string;
  lines: string[];
  fragment: DocumentFragment;
  blocked: number;
  counts: Record<string, number>;
}

/* What a rendered document holds, by kind, counted in the finished fragment so the count is what the reader sees. */
const KINDS: [string, string][] = [
  ["heading", "h1,h2,h3,h4,h5,h6"],
  ["anchor", "h1[id],h2[id],h3[id],h4[id],h5[id],h6[id]"],
  ["paragraph", "p"],
  ["list", "ul,ol"],
  ["table", "table"],
  ["task", "input.task-list-item-checkbox"],
  ["strikethrough", "s,del"],
  ["footnote", ".footnote-backref"],
  ["blockquote", "blockquote"],
  ["code", "pre"],
  ["math", "[data-eitri-math]"],
  ["mermaid", "[data-eitri-mermaid]"],
  ["alert", ".markdown-alert"],
  ["frontmatter", ".front-matter"],
  ["image", "img"],
  ["blocked", ".mdp-box[data-eitri-blocked]"],
  ["error", ".mdp-error-badge"],
];

function countKinds(fragment: DocumentFragment, env: RenderEnv): Record<string, number> {
  const counts: Record<string, number> = { ...env.counts };
  for (const [kind, selector] of KINDS) counts[kind] = fragment.querySelectorAll(selector).length;
  return counts;
}

/** Renders a buffer's text into a detached, sanitised fragment: parse and render with a fresh nonce, sanitise,
 *  then fill math and the diagrams already drawn while the fragment is still off the page, so the page swaps in a
 *  finished document. */
export function renderDocument(engine: Engine, text: TextBody, doc: DocPrefixes | null, settings: PreviewSettings,
  mermaid: MermaidRunner): RenderResult {
  // nvim keeps a byte-order mark out of the buffer, but a file read without 'bomb' handling can start with one; it
  // is one character on line 0, so dropping it moves no line.
  const source = text.text.startsWith("\uFEFF") ? text.text.slice(1) : text.text;
  const env: RenderEnv = { nonce: newNonce(), doc, remoteImages: settings.remote_images, blocked: [], counts: {} };
  const html = engine.render(source, env);
  const fragment = sanitizeDocument(html, {
    nonce: env.nonce, doc, remoteImages: env.remoteImages, blocked: env.blocked,
  });
  fillMath(fragment);
  mermaid.fill(fragment);
  const counts = countKinds(fragment, env);
  trace("rendered", {
    buf: text.buf, tick: text.tick, kinds: Object.keys(counts).filter((k) => counts[k] > 0).sort(),
    math: counts.math ?? 0, mermaid: counts.mermaid ?? 0, alerts: counts.alert ?? 0,
    frontmatter: counts.frontmatter ?? 0, images_shown: counts.image ?? 0, images_blocked: env.blocked.length,
  });
  // The lines keep any `\r` a CRLF file left on them: the copy code strips it, and splitting on `\n` alone is what
  // keeps their numbers equal to nvim's.
  return { buf: text.buf, tick: text.tick, name: text.name, lines: source.split("\n"), fragment,
    blocked: env.blocked.length, counts };
}
