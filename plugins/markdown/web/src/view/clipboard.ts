import { kit } from "../kit";
import { trace } from "../trace";
import { currentBlock } from "./current";
import { HUD } from "./notices";
import type { Preview } from "./preview";
import { yankText } from "./yank";

/** A hidden textarea and `execCommand("copy")`, synchronously; the element that had the focus gets it back. */
function execCopy(text: string): boolean {
  if (typeof document.execCommand !== "function") return false;
  const before = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand("copy") === true;
  } catch {
    ok = false;
  }
  area.remove();
  before?.focus();
  return ok;
}

/** Puts `text` on the clipboard from inside the key event that asked for it. Both paths start before the first
 *  `await`, while the key's user activation holds: `execCommand("copy")` first, whose answer is known at once, then
 *  `navigator.clipboard.writeText` only when that failed. True when one worked. */
export async function copyText(text: string): Promise<boolean> {
  if (execCopy(text)) return true;
  const clip = typeof navigator !== "undefined" ? navigator.clipboard : undefined;
  if (!clip || typeof clip.writeText !== "function") return false;
  try {
    await clip.writeText(text);
    return true;
  } catch {
    return false;
  }
}

export const COPY_FAILED = "could not copy to the clipboard";
const FENCE_LINE = /^[ \t]*(`{3,}|~{3,})/;

/** `yy`: the current block's Markdown from the snapshot the preview rendered; a fenced block's code without its
 *  fences. The HUD says how many lines went to the clipboard. */
export async function yankCurrentBlock(p: Preview): Promise<void> {
  const box = currentBlock(p);
  if (!box) return;
  const lines = p.lines();
  const fence = box.el.tagName === "PRE" && FENCE_LINE.test(lines[box.line] ?? "");
  const text = yankText(lines, { line: box.line, end: box.end, fence });
  const count = text === "" ? 0 : text.split("\n").length;
  const ok = await copyText(text);
  kit()?.message(ok ? HUD.yanked(count) : COPY_FAILED);
  trace("yanked", { lines: ok ? count : 0 });
}
