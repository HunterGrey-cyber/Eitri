import { kit } from "../kit";
import { openLink } from "../protocol";
import { anchorId, classifyHref, lineFragment, namesDocument, resolveDocument, splitFragment } from "../render/links";
import { jumpTo } from "./keys";
import { HUD } from "./notices";
import type { Preview } from "./preview";

/** Clicks on links in the document: an anchor scrolls the preview (a history entry); a local link opens the file in
 *  nvim, the keys staying where they are, and a fragment that is not a line is scrolled to once that file renders
 *  (at once when the link names the document already shown, which renders nothing new); an external link is left to
 *  its default, which the host turns into an outside launch; anything else does nothing. */
export function installLinks(p: Preview, root: HTMLElement): () => void {
  let waiting: { fragment: string; destination: string; origin: string; opened: boolean; rendered: Set<string> } | null = null;
  let clicks = 0;
  const scrollTo = (fragment: string) => {
    const el = document.getElementById(anchorId(fragment));
    if (el) jumpTo(p, el);
  };
  const finish = () => {
    if (!waiting) return;
    const pending = waiting;
    const name = p.shownName();
    // The shown name may differ from the hint through a symlink or an existing buffer's name. Judge these renders
    // once the host has returned the name it actually opened, retaining intervening navigation too.
    if (!waiting.opened) { waiting.rendered.add(name); return; }
    if ([...pending.rendered].some(path => path !== pending.origin && path !== pending.destination)) {
      waiting = null;
      return;
    }
    pending.rendered.clear();
    if (name === waiting.destination) {
      const { fragment } = waiting;
      waiting = null;
      scrollTo(fragment);
    } else if (name !== waiting.origin) waiting = null;
  };
  const off = p.afterUpdate((why) => { if (why === "render") finish(); });
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
    const destination = resolveDocument(path, name, p.shownDoc());
    // A newer click replaces whatever an older one was still waiting for, even when its answer comes later.
    const click = ++clicks;
    const pending = anchor && destination
      ? { fragment: anchor, destination, origin: name, opened: false, rendered: new Set<string>() } : null;
    waiting = pending;
    openLink(p.bridge, { buf, target: href }).then(
      (opened) => {
        if (!anchor || click !== clicks || waiting !== pending) return;
        if (pending) {
          pending.destination = opened.path ?? pending.destination;
          pending.opened = true;
          finish();
          if (waiting !== pending) return;
        }
        // nvim's current file was the target and the preview shows that buffer, or the link names the shown file:
        // either way no new text comes, so the fragment is reached now.
        const here = p.followedBuf() === buf && p.shownBuf() === buf &&
          ((opened.how === "current" && p.isCurrent()) || (opened.path !== undefined ? opened.path === name : namesDocument(path, name)));
        if (here) { waiting = null; scrollTo(anchor); }
      },
      (error: { code?: string; message?: string }) => {
        if (click === clicks) waiting = null;
        kit()?.message(error?.code === "outside_roots" ? HUD.outside : error?.code === "io" ? HUD.noSuchFile : error?.message ?? String(error));
      },
    );
  };
  root.addEventListener("click", onClick);
  return () => { off(); root.removeEventListener("click", onClick); };
}
