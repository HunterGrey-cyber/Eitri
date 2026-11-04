import type { TooLarge } from "../protocol";
import type { BlockReason } from "../render/env";

export const EMPTY_TEXT = "Open a Markdown file in the editor.";
export const BUFFER_CLOSED_TEXT = "The buffer was closed.";
export const EDITOR_CLOSED_TEXT = "The editor is closed.";

/* MiB, written MB as most readers know it, one decimal, `.0` dropped. */
function size(bytes: number): string {
  const mib = Math.round((bytes / (1024 * 1024)) * 10) / 10;
  return `${mib.toFixed(1).replace(/\.0$/, "")} MB`;
}

/* Thousands separated by commas, as en-US writes them, whatever the locale the page runs in. */
function count(n: number): string {
  return String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

export function tooLargeText(t: TooLarge): string {
  return `This document is too large to preview (${size(t.bytes)}, ${count(t.lines)} lines; ` +
    `the limit is ${size(t.max_bytes)} or ${count(t.max_lines)} lines)`;
}

const BLOCKED: Record<BlockReason, string> = {
  http: "http image not loaded",
  remote_off: "remote images are off",
  outside: "outside the project",
  unsaved: "save the file to show local images",
};

export function blockedText(reason: BlockReason): string {
  return BLOCKED[reason];
}

/** The HUD's right-hand segment: the followed file, whether nvim's current buffer is another one, and how many
 *  images were not loaded. */
export function statusText(s: { name: string; current: boolean; blocked: number }): string {
  let text = s.name === "" ? "[No Name]" : s.name.split("/").pop() || s.name;
  if (!s.current) text += " [not current]";
  if (s.blocked > 0) text += ` · ${s.blocked} image${s.blocked === 1 ? "" : "s"} not loaded`;
  return text;
}

export const HUD = {
  notOpen: "the file is not open in a window",
  notSelected: "the editor did not take the selection (is it in a prompt, or the file not open?)",
  outside: "outside the project",
  noSuchFile: "no such file",
  wrapBottom: "search hit BOTTOM, continuing at TOP",
  wrapTop: "search hit TOP, continuing at BOTTOM",
  yanked(lines: number): string { return `yanked ${lines} line${lines === 1 ? "" : "s"}`; },
};

/** The kit's find messages in the page's words: vim's wrap texts and the position as `[i/n]`. The kit shows a wrap as
 *  the wrap text, a space, then the position, as vim does. */
export const FIND_MESSAGES = {
  "find.wrapBottom": HUD.wrapBottom,
  "find.wrapTop": HUD.wrapTop,
  "find.count": "[{index}/{total}]",
};
