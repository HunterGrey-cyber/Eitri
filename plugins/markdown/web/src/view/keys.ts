import { jump } from "../protocol";
import { kit, type Row, type VisualKind } from "../kit";
import { copyText, COPY_FAILED } from "./clipboard";
import { sourceSelection } from "./selection";
import { boxAt, currentBlock, noteClick, pickedLine } from "./current";
import { lineToY, yToLine } from "./sync";
import { FIND_MESSAGES, HUD, statusText } from "./notices";
import type { Preview } from "./preview";

export const PAGE_CONTEXT = "preview";
/* The page's own reading keys. The kit declares its generic rows first (scrolling, half pages, history, find, hints);
   a key named here is taken from the generic row that had it, so `]]`/`[[` leave `link.next`/`link.prev` keyless
   and those rows drop out, and the cursor's keys leave the kit's scrolling `<C-e>`/`<C-y>` alone (they scroll the
   page, and the cursor stays on its row unless a margin pushes it, as in nvim). */
export const PAGE_ROWS: Row[] = [
  { action: "cursor.down", keys: ["j"], context: PAGE_CONTEXT, desc: "cursor a row down" },
  { action: "cursor.up", keys: ["k"], context: PAGE_CONTEXT, desc: "cursor a row up" },
  { action: "cursor.half_down", keys: ["d", "<C-d>"], context: PAGE_CONTEXT, desc: "half a page down" },
  { action: "cursor.half_up", keys: ["u", "<C-u>"], context: PAGE_CONTEXT, desc: "half a page up" },
  { action: "cursor.page_down", keys: ["<C-f>"], context: PAGE_CONTEXT, desc: "a page down" },
  { action: "cursor.page_up", keys: ["<C-b>"], context: PAGE_CONTEXT, desc: "a page up" },
  { action: "cursor.first", keys: ["gg"], context: PAGE_CONTEXT, desc: "first row" },
  { action: "cursor.last", keys: ["G"], context: PAGE_CONTEXT, desc: "last row" },
  { action: "heading.next", keys: ["]]"], context: PAGE_CONTEXT, desc: "next heading" },
  { action: "heading.prev", keys: ["[["], context: PAGE_CONTEXT, desc: "previous heading" },
  { action: "scroll.left", keys: ["zh"], context: PAGE_CONTEXT, desc: "scroll the block left" },
  { action: "scroll.right", keys: ["zl"], context: PAGE_CONTEXT, desc: "scroll the block right" },
  { action: "block.yank", keys: ["yy"], context: PAGE_CONTEXT, desc: "copy the block's Markdown" },
  { action: "edit.here", keys: ["<CR>"], context: PAGE_CONTEXT, desc: "edit here in the editor" },
];

/** The page's rows for the window's prefix (`host.hello`'s spelling, `Ctrl+b`): a key equal to the prefix is
 *  dropped, as the kit drops it from its own rows (the host refuses a table holding the prefix), and without a prefix
 *  every Ctrl key is, since the page cannot tell which one the window keeps. A row left with no key is not declared. */
export function pageRows(prefix: string | null | undefined): Row[] {
  const known = typeof prefix === "string";
  const banned = known ? /^Ctrl\+([a-z])$/.exec(prefix)?.[1] ?? null : null;
  return PAGE_ROWS.flatMap((row) => {
    const keys = row.keys.filter((key) => {
      const ctrl = /^<C-(.)>$/i.exec(key);
      return !ctrl || (known && ctrl[1] !== banned);
    });
    if (!keys.length) return [];
    return [keys.length === row.keys.length ? row : { ...row, keys }];
  });
}

/* What `/` leaves out of the document: KaTeX's hidden MathML (the formula is read once, as drawn) and a drawn
   diagram's text, which sits in SVG nobody reads as prose. */
export const FIND_EXCLUDE = ".katex-mathml, .mdp-diagram svg";

/* One step of sideways scrolling: a few characters of code, enough to make progress without losing the column. */
const SIDEWAYS_PX = 60;
const WIDE = ".mdp-wide, .katex-display";
const HEADING = /^H[1-6]$/;
/** How far into a target the cursor looks for its first row: a target's top can lie on the boundary with the row
 *  above it. */
const ROW_PROBE_PX = 0.5;

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
    const picked = y === undefined ? pickedLine(box, p.cursorMoves()) : null;
    // nvim's held line answers the reading line, so it stands for the current block only when that block is the one
    // at the reading point (not a clicked one elsewhere).
    const held = y === undefined && boxAt(index, at)?.el === box.el ? p.heldLine(at) : null;
    // The picked line is nvim's own, inside the block or on a line next to it no block holds.
    const line = picked !== null && picked >= 0 && picked < p.lines().length ? picked
      : held !== null ? held
      : inside || y === undefined ? (p.sourceLineAt(at) ?? box.line) : box.line;
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

/** Visual mode's Enter: the selection goes to nvim, selected there in its own visual mode, with the keys. Charwise
 *  when the selection lies inside one source line and its text occurs there exactly once, else linewise over the
 *  source lines it spans; nvim's cursor ends on the line of the selection's cursor, which the preview's cursor
 *  follows, so nvim's view moves no more than the selection needs. The selection is read at once, inside the key
 *  event, before visual mode ends. A host that does not know `select` gets the plain jump to the cursor's line. */
export function editSelection(p: Preview, kind: VisualKind): void {
  const buf = p.shownBuf();
  const sel = document.getSelection();
  const src = buf === null || !sel ? null : sourceSelection(p, sel, kind);
  if (buf === null || !src) return;
  p.beforeEditJump();
  const select = { line: src.focus.line + 1, col: src.focus.col, mode: src.mode };
  const ask = jump(p.bridge, { buf, line: src.anchor.line + 1, col: src.anchor.col, focus: true, select })
    .catch((error: unknown) => {
      if ((error as { code?: unknown } | null)?.code !== "bad_request") throw error;
      return jump(p.bridge, { buf, line: select.line, col: select.col, focus: true });
    });
  void ask.then((result) => result.applied, () => false).then((applied) => {
    if (applied) return;
    p.editJumpFailed();
    kit()?.message(HUD.notSelected);
  });
}

/** Visual mode's `Y`: the Markdown of the source lines the selection spans, from the snapshot the page rendered. */
export function yankSelection(p: Preview, kind: VisualKind): void {
  const sel = document.getSelection();
  const src = sel ? sourceSelection(p, sel, kind) : null;
  if (!src) return;
  const top = Math.min(src.anchor.line, src.focus.line);
  const bottom = Math.max(src.anchor.line, src.focus.line);
  const text = p.lines().slice(top, bottom + 1).map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l)).join("\n");
  const count = bottom - top + 1;
  void copyText(text).then((ok) => kit()?.message(ok ? HUD.yanked(count) : COPY_FAILED));
}

/* A history entry: the buffer, the line at the window's top `y` (where an entry without a cursor places the page),
   and the cursor's place. */
function entry(p: Preview, y: number, cursor: number | null): { mdp: true; buf: number | null; line: number; cursor?: number } {
  const line = yToLine(p.index(), y);
  return cursor === null ? { mdp: true, buf: p.shownBuf(), line } : { mdp: true, buf: p.shownBuf(), line, cursor };
}

/** Records the place a jump leaves (the window at `y`, the cursor at `cursor`): the current history entry says it. */
function leaving(p: Preview, y: number, cursor: number | null): void {
  try {
    history.replaceState(entry(p, y, cursor), "");
  } catch {
    // The engine limits how often history may change; a lost entry costs only that step of H.
  }
}

/** Records the place a jump reached: a new history entry. */
function arriving(p: Preview, y: number, cursor: number | null): void {
  try {
    history.pushState(entry(p, y, cursor), "");
  } catch {
    // As in `leaving`.
  }
}

/** Runs `move`, a jump of the cursor, and records the place left and the place reached as history entries, so `H`
 *  and `L` move between them; a jump that moved the cursor less than a pixel records nothing. */
function jumping(p: Preview, move: () => boolean): void {
  const fromY = p.destination();
  const fromBand = p.cursorBand();
  const from = p.cursorPlace();
  if (!move()) return;
  const toBand = p.cursorBand();
  if (!fromBand || !toBand || Math.abs(toBand.top - fromBand.top) < 1) return;
  leaving(p, fromY, from);
  arriving(p, p.destination(), p.cursorPlace());
}

/** An in-document jump (an anchor, a footnote): with the keys in the preview the cursor goes to the target's first
 *  row, the page scrolling only when that row is outside the margins; without them (a link followed while nvim has
 *  the keys) the target lands at the reading reserve below the top. The place left and the place reached become
 *  history entries. Entries carry the buffer; one from another buffer is ignored. */
export function jumpTo(p: Preview, el: Element): void {
  if (p.cursorBand()) {
    const y = el.getBoundingClientRect().top + window.scrollY;
    jumping(p, () => p.jumpCursorTo(y + ROW_PROBE_PX));
    return;
  }
  const from = p.destination();
  const y = p.scrollToElement(el);
  if (Math.abs(y - from) < 1) return;
  leaving(p, from, null);
  arriving(p, y, null);
}

/* A wide block (code, a table, a diagram, a formula) scrolls inside itself; by keys this is the only way to read
   what is past its right edge. */
function sideways(p: Preview, dx: number): void {
  const box = currentBlock(p);
  const wide = box ? (box.el.closest(WIDE) ?? box.el.querySelector(WIDE)) : null;
  wide?.scrollBy({ left: dx, behavior: "smooth" });
}

/* A preview has no next page to go to, so `]]`/`[[` move between sections, as they do in vim's own Markdown
   buffers: the cursor goes to the next heading below its row (the previous one above), and the page scrolls only
   when that heading is outside the margins. The move is a jump: the place left and the heading reached become
   history entries. */
function heading(p: Preview, dir: 1 | -1): void {
  const band = p.cursorBand();
  if (!band) return;
  const headings = p.index().filter((b) => HEADING.test(b.el.tagName));
  // A heading the cursor's band overlaps is the one it is on (the band, a line box, reaches a few px past the
  // heading's own box), never the next or the previous one.
  const target = dir > 0 ? headings.find((b) => b.top >= band.bottom - 1)
    : [...headings].reverse().find((b) => b.top + b.height <= band.top + 1);
  if (target) jumping(p, () => p.jumpCursorTo(target.top + ROW_PROBE_PX));
}

/* The match the kit's find moved to: its current highlight (the CSS Custom Highlight API), or the selection the kit
   shows it as without that API. */
function findMatch(): Range | null {
  const registry = (globalThis as { CSS?: { highlights?: { get(name: string): Iterable<unknown> | undefined } } }).CSS
    ?.highlights;
  if (registry) {
    for (const r of registry.get("eitri-find-current") ?? []) if (r instanceof Range) return r;
    return null;
  }
  const selection = document.getSelection();
  return selection && selection.rangeCount > 0 ? selection.getRangeAt(0) : null;
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
      case "cursor.down": p.moveRows(1); return true;
      case "cursor.up": p.moveRows(-1); return true;
      case "cursor.half_down": p.halfPage(1); return true;
      case "cursor.half_up": p.halfPage(-1); return true;
      case "cursor.page_down": p.fullPage(1); return true;
      case "cursor.page_up": p.fullPage(-1); return true;
      // gg and G are jumps, as in vim; a held key's repeats move nothing more.
      case "cursor.first": if (!ev.repeat) jumping(p, () => p.cursorFirst()); return true;
      case "cursor.last": if (!ev.repeat) jumping(p, () => p.cursorLast()); return true;
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
  // A click puts the cursor on the clicked row, without scrolling; a click on a link, a toggle or a check box is
  // that control's own.
  const onClick = (ev: MouseEvent) => {
    if ((ev.target as Element).closest?.("a, summary, input")) return;
    noteClick();
    p.cursorToPoint(ev.pageY);
    refresh();
  };
  const onDouble = (ev: MouseEvent) => { void editHere(p, ev.pageY); };
  // `H`/`L` are the browser's own back and forward; the page answers only the entries it pushed for the buffer it
  // shows now, so a jump made in another document never scrolls this one.
  const onPop = (ev: PopStateEvent) => {
    const state = ev.state as { mdp?: boolean; buf?: number; line?: number; cursor?: number } | null;
    if (!state?.mdp || state.buf !== p.shownBuf() || typeof state.line !== "number") return;
    // The entry's cursor goes back as a jump puts it; an entry the cursor did not record places the page by its line.
    if (typeof state.cursor === "number" && p.jumpCursorToLine(state.cursor)) return;
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
  // The kit's own jumps (a search's move) put the cursor on the match and go into the history the way `]]` and
  // anchors do. The kit has started its own scroll to the match; the cursor's jump replaces it, so the page moves
  // only when the match is outside the margins, judged from where the window was before.
  let jumpFrom: { y: number; cursor: number | null; band: number | null } | null = null;
  const onJump = (when: "before" | "after") => {
    if (when === "before") {
      jumpFrom = { y: p.destination(), cursor: p.cursorPlace(), band: p.cursorBand()?.top ?? null };
      return;
    }
    const from = jumpFrom;
    jumpFrom = null;
    const rect = findMatch()?.getBoundingClientRect();
    if (!from || !rect || !p.jumpCursorTo(rect.top + window.scrollY + rect.height / 2, from.y)) return;
    const to = p.cursorBand()?.top ?? null;
    if (from.band === null || to === null || Math.abs(to - from.band) < 1) return;
    leaving(p, from.y, from.cursor);
    arriving(p, p.destination(), p.cursorPlace());
  };
  const remove = k
    ? await k.install({
      rows: pageRows(extra.prefix),
      run,
      initial: { timeoutlen: extra.timeoutlen ?? 1000, hasKeys: p.hasKeys(), prefix: extra.prefix ?? null,
        settings: extra.settings },
      labels: { messages: FIND_MESSAGES },
      find: { root: () => root, exclude: FIND_EXCLUDE },
      onJump,
      // Visual mode starts at the cursor, the cursor follows its moving end (so nvim follows too), and Enter and `Y`
      // act on the source it spans.
      visual: {
        start: () => p.visualStart(),
        follow: (rect) => { p.followSelection(rect); refresh(); },
        keys: {
          "<CR>": (kind) => { editSelection(p, kind); return true; },
          Y: (kind) => { yankSelection(p, kind); return true; },
        },
      },
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
