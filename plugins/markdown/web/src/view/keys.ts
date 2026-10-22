import { jump } from "../protocol";
import { kit, type Row } from "../kit";
import { boxAt, currentBlock, setClickedBlock } from "./current";
import { lineToY, yToLine } from "./sync";
import { FIND_MESSAGES, HUD, statusText } from "./notices";
import type { Preview } from "./preview";

export const PAGE_CONTEXT = "preview";
/* The page's own reading keys. The kit declares its generic rows first (scrolling, half pages, history, find, hints);
   a key named here is taken from the generic row that had it, so `]]`/`[[` leave `link.next`/`link.prev` keyless and
   those rows drop out. */
export const PAGE_ROWS: Row[] = [
  { action: "heading.next", keys: ["]]"], context: PAGE_CONTEXT, desc: "next heading" },
  { action: "heading.prev", keys: ["[["], context: PAGE_CONTEXT, desc: "previous heading" },
  { action: "scroll.left", keys: ["zh"], context: PAGE_CONTEXT, desc: "scroll the block left" },
  { action: "scroll.right", keys: ["zl"], context: PAGE_CONTEXT, desc: "scroll the block right" },
  { action: "block.yank", keys: ["yy"], context: PAGE_CONTEXT, desc: "copy the block's Markdown" },
  { action: "edit.here", keys: ["<CR>"], context: PAGE_CONTEXT, desc: "edit here in the editor" },
];

/* What `/` leaves out of the document: KaTeX's hidden MathML (the formula is read once, as drawn) and a drawn
   diagram's text, which sits in SVG nobody reads as prose. */
export const FIND_EXCLUDE = ".katex-mathml, .mdp-diagram svg";

/* One step of sideways scrolling: a few characters of code, enough to make progress without losing the column. */
const SIDEWAYS_PX = 60;
const HEADING = /^H[1-6]$/;

/** Moves nvim's cursor to the source line at `y` (the reading position by default) and gives nvim the keys. Enter
 *  acts on the current block; a double-click passes the pointer's position, so the same jump lands where the reader
 *  pointed. Inside a tall block the line under the position is taken, not the block's first. */
export async function editHere(p: Preview, y?: number): Promise<void> {
  // The line is read from the shown blocks, so it is a line of the shown buffer.
  const buf = p.shownBuf();
  const index = p.index();
  const at = y ?? p.readingY();
  const box = y === undefined ? currentBlock(p) : boxAt(index, at);
  if (buf === null || !box) return;
  const inside = at >= box.top && at < box.top + box.height;
  const line = inside ? Math.min(Math.max(Math.floor(yToLine(index, at)), box.line), box.end - 1) : box.line;
  const result = await jump(p.bridge, { buf, line: line + 1, col: 0, focus: true });
  // nvim moves a cursor only in a window that shows the file; say so rather than leave the key looking dead.
  if (!result.applied) kit()?.message(HUD.notOpen);
}

/** An in-document jump (an anchor, a footnote): the place left and the place reached become history entries, so
 *  `H` and `L` move between them. Entries carry the buffer; one from another buffer is ignored. */
export function jumpTo(p: Preview, el: Element): void {
  const buf = p.shownBuf();
  history.replaceState({ mdp: true, buf, line: yToLine(p.index(), window.scrollY) }, "");
  p.scrollToElement(el, "top");
  const top = el.getBoundingClientRect().top + window.scrollY;
  history.pushState({ mdp: true, buf, line: yToLine(p.index(), top) }, "");
}

/* A wide block (code, a table, a diagram, a formula) scrolls inside itself; by keys this is the only way to read
   what is past its right edge. */
function sideways(p: Preview, dx: number): void {
  const box = currentBlock(p);
  const wide = box ? (box.el.closest(".mdp-wide") ?? box.el.querySelector(".mdp-wide")) : null;
  wide?.scrollBy({ left: dx, behavior: "smooth" });
}

/* A preview has no next page to go to, so `]]`/`[[` move between sections, as they do in vim's own Markdown
   buffers. */
function heading(p: Preview, dir: 1 | -1): void {
  const headings = p.index().filter((b) => HEADING.test(b.el.tagName));
  const y = window.scrollY;
  const target = dir > 0 ? headings.find((b) => b.top > y + 1) : [...headings].reverse().find((b) => b.top < y - 1);
  if (target) p.scrollToElement(target.el, "top");
}

export async function installKeys(
  p: Preview,
  extra: { run(action: string, ev: KeyboardEvent): boolean; timeoutlen?: number; prefix?: string; settings?: unknown },
): Promise<() => void> {
  const k = kit();
  const root = document.getElementById("mdp-root");
  // The bar beside the current block shows what `yy` and Enter act on; it means something only while the page holds
  // the keys.
  const bar = document.createElement("div");
  bar.className = "mdp-current-bar";
  bar.hidden = true;
  document.body.append(bar);
  let lastStatus = "";
  // Every view, scroll, render and keys change can move the current block or change the status, so both follow
  // each update; the status goes to the kit only when its text changed.
  const refresh = () => {
    const box = p.hasKeys() ? currentBlock(p) : null;
    bar.hidden = !box;
    if (box) {
      const left = (root?.getBoundingClientRect().left ?? 0) + window.scrollX - 12;
      Object.assign(bar.style, { top: `${box.top}px`, height: `${box.height}px`, left: `${left}px` });
    }
    const status = p.followedBuf() === null
      ? ""
      : statusText({ name: p.fileName(), current: p.isCurrent(), blocked: p.blockedCount() });
    if (status !== lastStatus) {
      lastStatus = status;
      k?.status(status);
    }
  };
  // The kit runs its generic actions itself (scrolling, history, hints); the page's own actions come here, and
  // what the page does not know (yank) goes to the caller.
  const run = (action: string, ev: KeyboardEvent): boolean => {
    switch (action) {
      case "heading.next": heading(p, 1); return true;
      case "heading.prev": heading(p, -1); return true;
      case "scroll.left": sideways(p, -SIDEWAYS_PX); return true;
      case "scroll.right": sideways(p, SIDEWAYS_PX); return true;
      case "edit.here": void editHere(p); return true;
      // Nothing of the page's is open for Esc to close; false lets the kit move focus off a focused link or toggle.
      case "mode.escape": return false;
      default: return extra.run(action, ev);
    }
  };
  // A click picks the block `yy` and Enter act on, without scrolling; a click on a link, a toggle or a check box is
  // that control's own.
  const onClick = (ev: MouseEvent) => {
    if ((ev.target as Element).closest?.("a, summary, input")) return;
    setClickedBlock(boxAt(p.index(), ev.pageY));
    refresh();
  };
  const onDouble = (ev: MouseEvent) => { void editHere(p, ev.pageY); };
  // `H`/`L` are the browser's own back and forward; the page answers only the entries it pushed for the buffer it
  // shows now, so a jump made in another document never scrolls this one.
  const onPop = (ev: PopStateEvent) => {
    const state = ev.state as { mdp?: boolean; buf?: number; line?: number } | null;
    if (!state?.mdp || state.buf !== p.shownBuf() || typeof state.line !== "number") return;
    const max = Math.max(0, document.documentElement.scrollHeight - window.innerHeight);
    window.scrollTo({ top: lineToY(p.index(), state.line, max), behavior: "instant" as ScrollBehavior });
  };
  // The browser would otherwise put back the pixel offset it saved with the entry, after the handler and whatever
  // buffer is shown: another buffer's offset in this one, or an offset the layout has since moved. Only the
  // handler's line, in the buffer it was recorded for, places the page.
  const restoration = history.scrollRestoration;
  history.scrollRestoration = "manual";
  root?.addEventListener("click", onClick);
  root?.addEventListener("dblclick", onDouble);
  window.addEventListener("popstate", onPop);
  // A render changes the text the kit's find walks; it finds again and keeps the current match near where it was.
  const off = p.afterUpdate((why) => {
    refresh();
    if (why === "render") k?.findRefresh();
  });
  const remove = k
    ? await k.install({
      rows: PAGE_ROWS,
      run,
      initial: { timeoutlen: extra.timeoutlen ?? 1000, hasKeys: p.hasKeys(), prefix: extra.prefix ?? null,
        settings: extra.settings },
      labels: { messages: FIND_MESSAGES },
      find: { root: () => root, exclude: FIND_EXCLUDE },
    })
    : () => {};
  refresh();
  return () => {
    remove();
    off();
    root?.removeEventListener("click", onClick);
    root?.removeEventListener("dblclick", onDouble);
    window.removeEventListener("popstate", onPop);
    history.scrollRestoration = restoration;
    bar.remove();
  };
}
