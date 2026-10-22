import { kit } from "../kit";
import { openLink } from "../protocol";
import { anchorId, classifyHref, lineFragment, namesDocument, splitFragment } from "../render/links";
import { jumpTo } from "./keys";
import { HUD } from "./notices";
import type { Preview } from "./preview";

/** Clicks on links in the document: an anchor scrolls the preview (a history entry); a local link opens the file in
 *  nvim, the keys staying where they are, and a fragment that is not a line is scrolled to once that file renders
 *  (at once when the link names the document already shown, which renders nothing new); an external link is left to
 *  its default, which the host turns into an outside launch; anything else does nothing. */
export function installLinks(p: Preview, root: HTMLElement): () => void {
  let waiting: { fragment: string; buf: number } | null = null;
  let clicks = 0;
  const scrollTo = (fragment: string) => {
    const el = document.getElementById(anchorId(fragment));
    if (el) jumpTo(p, el);
  };
  const off = p.afterUpdate((why) => {
    // Only a render of another buffer is the opened file's; an edit re-rendering the document clicked in is not.
    if (why !== "render" || !waiting || p.shownBuf() === waiting.buf) return;
    const { fragment } = waiting;
    waiting = null;
    scrollTo(fragment);
  });
  const onClick = (ev: MouseEvent) => {
    const a = (ev.target as Element | null)?.closest?.("a[href]");
    if (!a || !root.contains(a)) return;
    const href = a.getAttribute("href") ?? "";
    const kind = a.getAttribute("data-eitri-link") ?? classifyHref(href);
    if (kind === "external") return;
    ev.preventDefault();
    (a as HTMLElement).blur();
    if (kind === "anchor") {
      scrollTo(href.slice(1));
      return;
    }
    // The link was clicked in the shown document, so it is resolved against that file even while nvim already
    // follows another buffer whose text has not rendered yet.
    const buf = p.shownBuf();
    if (kind !== "local" || buf === null) return;
    const { path, fragment } = splitFragment(href);
    const anchor = fragment && lineFragment(fragment) === null ? fragment : null;
    const name = p.shownName();
    // A newer click replaces whatever an older one was still waiting for, even when its answer comes later.
    const click = ++clicks;
    waiting = null;
    openLink(p.bridge, { buf, target: href }).then(
      (opened) => {
        if (!anchor || click !== clicks) return;
        // nvim's current file was the target and the preview shows that buffer, or the link names the shown file:
        // either way no new text comes, so the fragment is reached now.
        const here = p.followedBuf() === buf && p.shownBuf() === buf && ((opened.how === "current" && p.isCurrent()) || namesDocument(path, name));
        if (here) scrollTo(anchor);
        else waiting = { fragment: anchor, buf };
      },
      (error: { code?: string; message?: string }) => {
        kit()?.message(error?.code === "outside_roots" ? HUD.outside : error?.code === "io" ? HUD.noSuchFile : error?.message ?? String(error));
      },
    );
  };
  root.addEventListener("click", onClick);
  return () => { off(); root.removeEventListener("click", onClick); };
}
