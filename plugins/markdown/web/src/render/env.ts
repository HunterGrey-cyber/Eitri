import type { DocPrefixes } from "../protocol";

export type { DocPrefixes } from "../protocol";

/** The attribute that marks an element the renderer made, as against one the document wrote. */
export const NONCE_ATTR = "data-eitri-n";
/** On a fenced code block: how many lines of code it holds, the lines after its opening fence line. */
export const FENCE_ATTR = "data-eitri-fence";
/** Renderer-owned boundaries of a raw HTML block, kept only after the nonce check. */
export const RAW_HTML_ATTR = "data-eitri-html";

export type BlockReason = "http" | "remote_off" | "outside" | "unsaved";

export interface RenderEnv {
  nonce: string;
  rawHtmlBlocks?: number;
  doc: DocPrefixes | null;
  remoteImages: "https" | "off";
  blocked: { reason: BlockReason; alt: string }[];
  counts: Record<string, number>;
}

/** A fresh value per render, so a document cannot guess it and write the mark itself. */
export function newNonce(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Written after every piece of the document's raw HTML. Raw HTML may stop part-way through a tag (`<div
 *  class="x"` and a blank line); the parser would then read the renderer's next tag as more of the document's, and
 *  the document's element would pick up that tag's nonce and keep its own forged attributes. Whatever state the
 *  HTML tokenizer is left in -- inside a tag, an attribute value of either quote or none, an end tag, a comment, a
 *  declaration -- this ends it: in a double-quoted value the `"` closes it, in a single-quoted one the `'`, and the
 *  `>` then ends the tag; a bogus comment or declaration ends at that `>`; an open comment ends at `-->`. After
 *  ordinary HTML it is an empty comment, which the sanitiser drops. Only inside raw text (an unclosed `textarea`,
 *  `title`, `plaintext`) does it not return to markup, and there no tag of the renderer's forms an element at all. */
export const RAW_HTML_END = `<!--"'>-->`;

export function mark(env: { nonce: string }): string {
  return ` ${NONCE_ATTR}="${env.nonce}"`;
}
