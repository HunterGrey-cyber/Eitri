import { pathAt } from "./paths";
import type { PathRef } from "./paths";
import { conversationRows } from "./nav";
import { isChromeNode, isUnrenderedNode } from "./visual";

export type PathOffsets = { start: number; end: number };
export type PathTarget = PathOffsets & { kind: "path"; rowKey: string; text: string };

type RowText = { text: string; nodes: { node: Text; start: number; end: number }[] };

/** The drawn text's offsets exclude chrome and folded content, including across inline spans. */
function rowText(row: HTMLElement): RowText {
  const nodes: RowText["nodes"] = [];
  let text = "";
  if (isUnrenderedNode(row, row.ownerDocument)) return { text, nodes };
  const walker = row.ownerDocument.createTreeWalker(row, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
    if (isChromeNode(node, row) || isUnrenderedNode(node, row)) continue;
    const start = text.length;
    text += (node as Text).data;
    nodes.push({ node: node as Text, start, end: text.length });
  }
  return { text, nodes };
}

/** Capture the selection's intersection with a row before leaving its frozen DOM. */
export function pathSelectionOffsets(row: HTMLElement, selection: Range): PathOffsets | null {
  let start: number | null = null;
  let end = 0;
  for (const part of rowText(row).nodes) {
    if (selection.comparePoint(part.node, 0) > 0 || selection.comparePoint(part.node, part.node.length) < 0) continue;
    const lo = selection.startContainer === part.node ? selection.startOffset : 0;
    const hi = selection.endContainer === part.node ? selection.endOffset : part.node.length;
    if (hi <= lo) continue;
    start ??= part.start + lo;
    end = part.start + hi;
  }
  return start === null ? null : { start, end };
}

const PATH_CHAR = /[\w.\-/]/;

/** Dots after a complete path are punctuation only when the whole run ends outside a path. */
function pathBoundary(text: string, start: number, end: number): boolean {
  if (start > 0 && PATH_CHAR.test(text[start - 1])) return false;
  let after = end;
  while (text[after] === ".") after++;
  return after === text.length || !PATH_CHAR.test(text[after]);
}

/** First drawn occurrence of each path in document order, optionally confined to selected offsets. */
export function pathRanges(row: HTMLElement, refs: PathRef[], selection?: PathOffsets | null): (PathOffsets | null)[] {
  const { text } = rowText(row);
  const claimed = new Set<number>();
  return refs.map((ref) => {
    const needle = ref.path + (ref.line === null ? "" : `:${ref.line}`);
    if (needle === "" || selection === null) return null;
    for (let at = text.indexOf(needle); at !== -1; at = text.indexOf(needle, at + 1)) {
      const end = at + needle.length;
      if (claimed.has(at) || !pathBoundary(text, at, end)) continue;
      if (ref.line === null && /^:\d/.test(text.slice(end))) continue;
      if (selection !== undefined && (at < selection.start || end > selection.end)) continue;
      claimed.add(at);
      return { start: at, end };
    }
    return null;
  });
}

/** Only timeline rows count; markup inside a reply cannot impersonate another row's key. */
export function pathRow(root: HTMLElement, rowKey: string): HTMLElement | null {
  return conversationRows(root).find((row) => row.getAttribute("data-row-key") === rowKey) ?? null;
}

/** Resolve against the current DOM, refusing a moved or changed path rather than labelling other text. */
export function resolvePathTarget(root: HTMLElement, target: PathTarget): Range | null {
  return pathTargetPass(root).resolve(target);
}

/** A placement pass shares row lookups and text walks, then discards them before the DOM can change. */
export function pathTargetPass(root: HTMLElement) {
  const rows = new Map<string, HTMLElement | null>();
  const texts = new Map<string, RowText>();
  const row = (key: string) => {
    if (!rows.has(key)) rows.set(key, pathRow(root, key));
    return rows.get(key)!;
  };
  return {
    row,
    resolve(target: PathTarget): Range | null {
      const el = row(target.rowKey);
      if (el === null || !el.isConnected) return null;
      let drawn = texts.get(target.rowKey);
      if (drawn === undefined) {
        drawn = rowText(el);
        texts.set(target.rowKey, drawn);
      }
      return rangeInRow(el, drawn, target);
    },
  };
}

function rangeInRow(row: HTMLElement, { text, nodes }: RowText, target: PathTarget): Range | null {
  const { start, end } = target;
  if (start < 0 || end <= start || text.slice(start, end) !== target.text || !pathBoundary(text, start, end)) return null;
  if (!/:\d+$/.test(target.text) && /^:\d/.test(text.slice(end))) return null;
  const first = nodes.find((part) => part.start <= start && part.end > start);
  const last = nodes.find((part) => part.start < end && part.end >= end);
  if (first === undefined || last === undefined) return null;
  const range = row.ownerDocument.createRange();
  range.setStart(first.node, start - first.start);
  range.setEnd(last.node, end - last.start);
  return range;
}

const PATH_BLOCK = "div, p, li, td, th, pre, h1, h2, h3, h4, h5, h6";

/** Read the caret's own block, keeping inline spans together and block boundaries apart. */
export function pathAtCaret(row: HTMLElement, node: Node, offset: number): PathRef | null {
  let block = node.parentElement;
  while (block !== null && block !== row && !block.matches(PATH_BLOCK)) block = block.parentElement;
  if (block === null || !row.contains(block)) return null;
  const walker = row.ownerDocument.createTreeWalker(block, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
  let text = "";
  let at: number | null = null;
  let lastBlock: Element | null = null;
  for (let part = walker.nextNode(); part !== null; part = walker.nextNode()) {
    if (isChromeNode(part, block) || isUnrenderedNode(part, block)) continue;
    if (part instanceof Element) {
      if (part.matches(PATH_BLOCK) || part.tagName === "BR") text += "\n";
      continue;
    }
    const ownBlock = part.parentElement?.closest(PATH_BLOCK) ?? block;
    if (lastBlock !== null && ownBlock !== lastBlock && !text.endsWith("\n")) text += "\n";
    lastBlock = ownBlock;
    if (part === node) at = text.length + offset;
    text += (part as Text).data;
  }
  return at === null ? null : pathAt(text, at);
}
