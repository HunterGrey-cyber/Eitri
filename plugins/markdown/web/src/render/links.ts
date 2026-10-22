export type LinkKind = "local" | "external" | "anchor" | "none";

const SCHEME = /^([a-zA-Z][a-zA-Z0-9+.-]*):/;

/** What following a link does: `external` (http, https, mailto) opens outside Eitri, `anchor` scrolls the preview,
 *  `local` opens a file of the project in nvim, and anything else (another scheme, a drive letter, a host without a
 *  scheme, a Windows share) does nothing at all. */
export function classifyHref(href: string): LinkKind {
  const h = href.trim();
  if (h === "") return "none";
  if (h.startsWith("#")) return "anchor";
  if (h.startsWith("//") || h.startsWith("\\")) return "none";
  const scheme = SCHEME.exec(h);
  if (scheme) {
    const name = scheme[1].toLowerCase();
    return name === "http" || name === "https" || name === "mailto" ? "external" : "none";
  }
  return "local";
}

export function splitFragment(target: string): { path: string; fragment: string | null } {
  const at = target.indexOf("#");
  return at < 0 ? { path: target, fragment: null } : { path: target.slice(0, at), fragment: target.slice(at + 1) };
}

/** The line a `#L10` or `#L10-L20` fragment names (GitHub's form), or null for any other fragment. */
export function lineFragment(fragment: string | null): number | null {
  if (fragment === null) return null;
  const m = /^L(\d+)(?:-L\d+)?$/.exec(fragment);
  if (!m) return null;
  const line = Number(m[1]);
  return line >= 1 ? line : null;
}

/** Whether a link's path, resolved as written against the shown document's own absolute name, is that document
 *  (`notes.md#usage` inside `/p/notes.md`). Opening it changes no text, so nothing renders afterwards to wait for. A
 *  path from the project root is not resolved: the page does not know where that root is. */
export function namesDocument(path: string, docName: string): boolean {
  if (path === "" || path.startsWith("/") || !docName.startsWith("/")) return false;
  let decoded: string;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    return false;
  }
  const parts = docName.split("/").slice(0, -1);
  for (const seg of decoded.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (parts.length > 1) parts.pop();
    } else {
      parts.push(seg);
    }
  }
  return parts.join("/") === docName;
}

/** The element id a `#fragment` names in the preview: GitHub's `user-content-` prefix (the sanitiser gives every
 *  document id that prefix), on the decoded fragment, never twice. */
export function anchorId(fragment: string): string {
  let decoded = fragment;
  try {
    decoded = decodeURIComponent(fragment);
  } catch {
    // A fragment that is not valid percent-encoding is used as written.
  }
  return decoded.startsWith("user-content-") ? decoded : `user-content-${decoded}`;
}
