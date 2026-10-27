import { jump } from "../protocol";
import { kit, type Row } from "../kit";
import { boxAt, currentBlock, lineAt, pickedLine, setClickedBlock } from "./current";
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
 *  pointed. Inside a tall block the line under the position is taken, not the block's first. A running glide ends
 *  where it was heading first, and nothing the preview sends afterwards moves nvim's cursor off the jump's line.
 *  Enter keeps the line nvim's cursor is already on when it was put there for this place, so the jump never scrolls
 *  nvim: the current block held nvim's cursor as the keys came in (or is the block nearest to it, when nvim's
 *  cursor was on a line no block holds, such as a blank line between paragraphs), or nvim answered the page's reveal
 *  of the reading line by putting its cursor on it, or on the nearest line it can hold without scrolling. That line
 *  may lie in the next block: nvim cannot hold its cursor on its top line under 'scrolloff', and a long wrapped
 *  paragraph straddling the top is that line. A jump to the line nvim's cursor is already on keeps its column as
 *  well, so it leaves the cursor exactly where it was; a reveal leaves the cursor at a line's first byte, and from
 *  there Enter lands on the line's first non-blank character. */
export async function editHere(p: Preview, y?: number): Promise<void> {
  // The line is read from the shown blocks, so it is a line of the shown buffer.
  const buf = p.shownBuf();
  if (buf === null) return;
  p.beforeEditJump();
  let applied = false;
  try {
    const index = p.index();
    const box = y === undefined ? currentBlock(p) : boxAt(index, y);
    if (!box) return;
    const at = y ?? p.readingY();
    const inside = at >= box.top && at < box.top + box.height;
    const picked = y === undefined ? pickedLine(box) : null;
    // nvim's held line answers the reading line, so it stands for the current block only when that block is the one
    // at the reading point (not a clicked one elsewhere).
    const held = y === undefined && boxAt(index, at)?.el === box.el ? p.heldLine(at) : null;
    // The picked line is nvim's own, inside the block or on a line next to it no block holds.
    const line = picked !== null && picked >= 0 && picked < p.lines().length ? picked
      : held !== null ? held
      : inside ? (lineAt(index, at) ?? box.line) : box.line;
    // On the line nvim's cursor is on, the column stays too: a cursor moved to the start of a long wrapped line
    // at nvim's top would scroll nvim. Elsewhere the first non-blank character, as nvim's own line jumps land. A
    // cursor at a line's first byte other than the one the keys came in with was put there by a reveal, which
    // places it at byte 0; it still lands on the first non-blank character, on the same screen row unless the
    // indentation itself wraps.
    const own = line === p.editorCursorLine() ? p.editorCursorCol() : null;
    const col = own !== null && (line === picked || own > 1) ? own : 0;
    const result = await jump(p.bridge, { buf, line: line + 1, col, focus: true });
    applied = result.applied;
    // nvim moves a cursor only in a window that shows the file; say so rather than leave the key looking dead.
    if (!applied) kit()?.message(HUD.notOpen);
  } finally {
    if (!applied) p.editJumpFailed();
  }
}

/** Records the window position `y` as the line left by a jump: the current history entry says it. */
function leaving(p: Preview, y: number): void {
  try {
    history.replaceState({ mdp: true, buf: p.shownBuf(), line: yToLine(p.index(), y) }, "");
  } catch {
    // The engine limits how often history may change; a lost entry costs only that step of H.
  }
}

/** Records the window position `y` as the place a jump reached: a new history entry. */
function arriving(p: Preview, y: number): void {
  try {
    history.pushState({ mdp: true, buf: p.shownBuf(), line: yToLine(p.index(), y) }, "");
  } catch {
    // As in `leaving`.
  }
}

/** An in-document jump (an anchor, a footnote): the target lands at the reading reserve below the top, through the
 *  kit's glide, and the place left and the place reached become history entries, so `H` and `L` move between them.
 *  Entries carry the buffer; one from another buffer is ignored. */
export function jumpTo(p: Preview, el: Element): void {
  leaving(p, p.destination());
  const y = p.scrollToElement(el);
  arriving(p, y);
}

/* A wide block (code, a table, a diagram, a formula) scrolls inside itself; by keys this is the only way to read
   what is past its right edge. */
function sideways(p: Preview, dx: number): void {
  const box = currentBlock(p);
  const wide = box ? (box.el.closest(".mdp-wide") ?? box.el.querySelector(".mdp-wide")) : null;
  wide?.scrollBy({ left: dx, behavior: "smooth" });
}

/* A preview has no next page to go to, so `]]`/`[[` move between sections, as they do in vim's own Markdown
   buffers. A heading lands at the reading reserve below the top, so the next one is looked for below that line, and
   from where a running glide is heading, so quick presses each move one more heading. The move is a jump: the line
   left and the heading reached become history entries. */
function heading(p: Preview, dir: 1 | -1): void {
  const headings = p.index().filter((b) => HEADING.test(b.el.tagName));
  const from = p.destination();
  const reading = from + p.reservePx();
  const target = dir > 0 ? headings.find((b) => b.top > reading + 1)
    : [...headings].reverse().find((b) => b.top < reading - 1);
  if (!target) return;
  const y = p.yFor(target.top);
  if (Math.abs(y - from) < 1) return;
  leaving(p, from);
  p.glideTo(y);
  arriving(p, y);
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
  // The kit's own jumps (gg, G, a search's move) go into the history the way `]]` and anchors do: the line left,
  // then the line reached.
  let jumpFrom: number | null = null;
  const onJump = (when: "before" | "after") => {
    if (when === "before") { jumpFrom = p.destination(); return; }
    const from = jumpFrom;
    jumpFrom = null;
    const to = p.destination();
    if (from === null || Math.abs(to - from) < 1) return;
    leaving(p, from);
    arriving(p, to);
  };
  const remove = k
    ? await k.install({
      rows: PAGE_ROWS,
      run,
      initial: { timeoutlen: extra.timeoutlen ?? 1000, hasKeys: p.hasKeys(), prefix: extra.prefix ?? null,
        settings: extra.settings },
      labels: { messages: FIND_MESSAGES },
      find: { root: () => root, exclude: FIND_EXCLUDE },
      bottom: () => p.bottomY(),
      onJump,
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
