import { NONCE_ATTR, type BlockReason, type DocPrefixes } from "./env";
import { blockedText } from "../view/notices";

export type ImageResult = { url: string } | { blocked: BlockReason };

/* Any scheme at all. A Windows drive letter (`C:`) matches it too, which is wanted: such a path names nothing the
 * page could serve. */
const SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;

/**
 * Where an image a document names may be loaded from, or why it is not.
 *
 * The order of the checks is what makes this safe, so it is fixed: the schemes the page may load are recognised
 * first and returned as written; everything else that could name another place (another scheme, a host-relative
 * `//`, a home `~`, a backslash, which a URL parser treats as a plain character here and other code may not) is
 * refused before any resolving happens; only then is the path resolved under the document route, and the result
 * must still sit under the project's prefix, which catches `..` and its percent-encoded spellings after the URL
 * parser has normalised them.
 */
export function resolveImage(src: string, doc: DocPrefixes | null, remote: "https" | "off"): ImageResult {
  const s = src.trim();
  if (/^data:image\//i.test(s)) return { url: s };
  if (/^https:/i.test(s)) return remote === "https" ? { url: s } : { blocked: "remote_off" };
  if (/^http:/i.test(s)) return { blocked: "http" };
  if (s === "" || SCHEME.test(s) || s.startsWith("//") || s.startsWith("~") || s.includes("\\")) {
    return { blocked: "outside" };
  }
  // An unnamed buffer has no directory, so a local path has nothing to be relative to until the file is saved.
  if (doc === null) return { blocked: "unsaved" };
  let url: string;
  try {
    // A leading slash means the project's root, as on a forge; the slashes are dropped so the URL parser does not
    // read the path as replacing the whole route.
    url = s.startsWith("/") ? new URL(s.replace(/^\/+/, ""), doc.root).href : new URL(s, doc.base).href;
  } catch {
    return { blocked: "outside" };
  }
  // The route's root itself is a directory, never an image.
  return url.startsWith(doc.root) && url.length > doc.root.length ? { url } : { blocked: "outside" };
}

const ESCAPES: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => ESCAPES[c]);
}

/**
 * The box drawn in place of an image that was not loaded: the reason, then the image's alt text. Both are the
 * document's or fixed text and are escaped; the box carries the render's nonce so the sanitiser keeps its class.
 */
export function boxHtml(alt: string, reason: BlockReason, nonce: string): string {
  const n = ` ${NONCE_ATTR}="${escapeHtml(nonce)}"`;
  const tail = alt === "" ? "" : ` ${escapeHtml(alt)}`;
  return `<span class="mdp-box" data-eitri-blocked="${escapeHtml(reason)}"${n}>` +
    `<span class="mdp-box-reason"${n}>${escapeHtml(blockedText(reason))}</span>${tail}</span>`;
}
