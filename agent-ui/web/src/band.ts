import type { PanelMode } from "./keymap";

/** A segment's identity, used for lookups.
 *  `bandLayout`'s construction order determines degrade order and render order. */
export type SegId = "mode" | "pill" | "showcmd" | "message" | "prompt" | "warn" | "unread" | "cards" | "queue" | "context" | "position" | "model";

/** One band segment's text and side of the gap. The left holds mode, pill, `⚑N`, `⧗N`, and
 *  message; the right holds showcmd, `⚠`, context, model, position, and `↓N`.
 *  `StatusBand` groups by `side` when rendering; `bandLayout` reorders only by priority. */
export type Seg = {
  id: SegId;
  text: string;
  side: "left" | "right";
  /** Set only on a prompt or message whose full text exceeds the band's measured row width.
   *  `StatusBand` wraps it at auto height so the trailing `(y/n)` or flash explanation stays
   *  visible. Absent on other segments, which still drop by priority. */
  wraps?: boolean;
};

/** What the band has to say this render, gathered by `App.tsx` from state that already exists
 *  elsewhere (this crate's own state, the tab's mode, the reducer's projection) -- `bandLayout` is
 *  a pure function over this plus the measured width, so the degrade rule is testable
 *  with no browser. */
export type BandFacts = {
  mode: PanelMode;
  pill: string;
  showcmd: string | null;
  message: string | null;
  prompt: string | null;
  warn: string | null;
  unread: string | null;
  cards: number;
  queued: number;
  context: { file: string; lines: [number, number] | null } | null;
  position: string | null;
  model: string | null;
};

/** Count these East Asian wide and fullwidth ranges as two cells so file and model names
 *  do not overrun the band's monospace budget. This is a subset of Unicode's width table:
 *  CJK ideographs and extensions, Hangul syllables and jamo, and fullwidth forms. */
const WIDE = /[ᄀ-ᅟ⺀-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]/u;

/** Character count with East Asian wide characters counted twice. Iterates by code
 *  point, not UTF-16 unit, so a codepoint outside the BMP is never split into two "wide" halves. */
export function textWidth(text: string): number {
  let n = 0;
  for (const ch of text) n += WIDE.test(ch) ? 2 : 1;
  return n;
}

/** Prompts and messages keep their full text so narrow bands cannot hide a trailing `(y/n)` or the
 *  explanation of how to answer. Oversized segments use `wraps: true`; `StatusBand` and
 *  `.status-band--wrap` let the band grow upward until the prompt or flash ends. Other segments
 *  still degrade by priority. */

const PAD = 2; // one character of padding each side (8px at --fs-sm)
// CARET, VISUAL, and V-LINE shorten to C, V, and VL when the band is narrow.
const MODE_TEXT: Record<PanelMode, string> = { input: "INPUT", browse: "BROWSE", hint: "HINT", caret: "CARET", visual: "VISUAL", vline: "V-LINE" };
const MODE_LETTER: Record<PanelMode, string> = { input: "I", browse: "B", hint: "H", caret: "C", visual: "V", vline: "VL" };

/** The bottom band's degrade rule, a pure function of the facts and the measured
 *  width so it is testable without a browser. `widthPx <= 0` (before the first measurement)
 *  or `charPx <= 0` (the hidden measuring span not yet laid out) shows only mode and pill. */
export function bandLayout(f: BandFacts, widthPx: number, charPx: number): Seg[] {
  const mode: Seg = { id: "mode", text: MODE_TEXT[f.mode], side: "left" };
  const pill: Seg = { id: "pill", text: f.pill, side: "left" };
  if (widthPx <= 0 || charPx <= 0) return [mode, pill];
  const budget = Math.floor(widthPx / charPx);
  if (f.prompt !== null) {
    const roomForPrompt = budget - textWidth(mode.text) - 2 * PAD;
    return [mode, { id: "prompt", text: f.prompt, side: "left", wraps: textWidth(f.prompt) > roomForPrompt }];
  }
  const ctxFull = f.context && (f.context.lines ? `⧉ ${f.context.file}:${f.context.lines[0]}-${f.context.lines[1]}` : `⧉ ${f.context.file}`);
  let segs: Seg[] = [
    mode,
    pill,
    ...(f.cards > 0 ? [{ id: "cards", text: `⚑${f.cards}`, side: "left" } as Seg] : []),
    ...(f.queued > 0 ? [{ id: "queue", text: `⧗${f.queued}`, side: "left" } as Seg] : []),
    ...(f.message ? [{ id: "message", text: f.message, side: "left" } as Seg] : []),
    ...(f.showcmd ? [{ id: "showcmd", text: f.showcmd, side: "right" } as Seg] : []),
    ...(f.warn ? [{ id: "warn", text: "⚠", side: "right" } as Seg] : []),
    ...(ctxFull ? [{ id: "context", text: ctxFull, side: "right" } as Seg] : []),
    ...(f.model ? [{ id: "model", text: f.model, side: "right" } as Seg] : []),
    ...(f.position ? [{ id: "position", text: f.position, side: "right" } as Seg] : []),
    ...(f.unread ? [{ id: "unread", text: f.unread, side: "right" } as Seg] : []),
  ];
  const width = (s: Seg[]) => s.reduce((n, x) => n + textWidth(x.text) + PAD, 0);
  const steps: ((s: Seg[]) => Seg[])[] = [
    (s) => s.map((x) => (x.id === "context" && f.context ? { ...x, text: `⧉ ${f.context.file}` } : x)),
    (s) => s.filter((x) => x.id !== "model"),
    (s) => s.filter((x) => x.id !== "position"),
    (s) => s.filter((x) => x.id !== "context"),
    (s) => s.filter((x) => x.id !== "queue"),
    (s) => s.filter((x) => x.id !== "cards"),
    (s) => s.map((x) => (x.id === "mode" ? { ...x, text: MODE_LETTER[f.mode] } : x)),
  ];
  for (const apply of steps) {
    if (width(segs) <= budget) break;
    segs = apply(segs);
  }
  // If the full message still exceeds the row after every droppable segment is gone, mark it
  // `wraps`. `StatusBand` grows to keep the flash's explanation visible.
  const over = width(segs) - budget;
  if (over > 0) segs = segs.map((x) => (x.id === "message" ? { ...x, wraps: true } : x));
  return segs;
}

/** The model without its leading `claude-`: `"claude-sonnet-5"` ->
 *  `"sonnet-5"`. A model that never carried the prefix (or `null`, before a session reports one)
 *  passes through unchanged. */
export function shortModel(model: string | null): string | null {
  if (model === null) return null;
  return model.startsWith("claude-") ? model.slice("claude-".length) : model;
}
