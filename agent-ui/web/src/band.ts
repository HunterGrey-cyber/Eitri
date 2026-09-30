import type { PanelMode } from "./keymap";
import type { UsageInfo } from "./types";

/** A segment's identity, used for lookups.
 *  `bandLayout`'s construction order determines degrade order and render order. */
export type SegId = "mode" | "pill" | "showcmd" | "message" | "prompt" | "warn" | "unread" | "cards" | "approve" | "queue" | "context" | "position" | "model" | "usage";

/** One band segment's text and side of the gap. The left holds mode, pill, `⚑N`, `⧗N`, and
 *  message; the right holds showcmd, `⚠`, context, model, usage, position, and `↓N`.
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
  /** The active tab's latest usage, formatted by `usageSegment`: `text` is the segment and `title`
   *  its tooltip. Null until a turn reports usage, and always null on an empty tab. */
  usage: UsageFact | null;
  /** A design decision (2026-09-30): the card INPUT's `Ctrl+y` would approve -- the active tab's
   *  oldest waiting card -- set by `App.tsx` only while one waits and the composer has the keys, so
   *  the band says which card that one key answers. Optional: absent (or `null`) is "nothing to
   *  name", which every caller that predates #39 means. */
  approve?: ApproveFact | null;
};

/** #39's segment: the card's tool name and a one-line summary of its input (`cardSummary`). */
export type ApproveFact = { tool: string; summary: string };

/** How long `cardSummary` lets a card's summary run on the band, the `…` included. */
const CARD_SUMMARY_MAX = 40;

/** A design decision: a card in one short line for the band -- what the call acts on (its command,
 *  file, path or URL, the fields a person checks before approving), else `summarizeInput`'s own
 *  order's remaining fields, else compact JSON; whitespace collapsed; cut to `CARD_SUMMARY_MAX`
 *  characters in the middle, head … tail. A `Bash` card's `description` is the
 *  model's own words about the command, so the command itself comes first. */
export function cardSummary(input: unknown): string {
  let text: string | null = null;
  if (input !== null && typeof input === "object") {
    const fields = input as Record<string, unknown>;
    for (const key of ["command", "file_path", "notebook_path", "path", "url", "pattern", "query", "description", "prompt", "name"]) {
      if (typeof fields[key] === "string") {
        text = fields[key] as string;
        break;
      }
    }
  }
  const line = (text ?? JSON.stringify(input) ?? "").replace(/\s+/g, " ").trim();
  const chars = Array.from(line);
  if (chars.length <= CARD_SUMMARY_MAX) return line;
  // Cut the middle so the summary keeps the command's tail. Options such as `--force` and
  // suffixes such as `| sudo bash` can matter when approving it.
  const tail = Math.floor((CARD_SUMMARY_MAX - 1) / 2) + 4;
  const head = CARD_SUMMARY_MAX - 1 - tail;
  return `${chars.slice(0, head).join("")}…${chars.slice(chars.length - tail).join("")}`;
}

/** The band's usage segment as `usageSegment` words it: the text drawn, and the tooltip carrying
 *  the breakdown the text has no room for. */
export type UsageFact = { text: string; title: string };

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
    ...(f.approve ? [{ id: "approve", text: `Ctrl+y approves ${f.approve.tool}: ${f.approve.summary}`, side: "left" } as Seg] : []),
    ...(f.queued > 0 ? [{ id: "queue", text: `⧗${f.queued}`, side: "left" } as Seg] : []),
    ...(f.message ? [{ id: "message", text: f.message, side: "left" } as Seg] : []),
    ...(f.showcmd ? [{ id: "showcmd", text: f.showcmd, side: "right" } as Seg] : []),
    ...(f.warn ? [{ id: "warn", text: "⚠", side: "right" } as Seg] : []),
    ...(ctxFull ? [{ id: "context", text: ctxFull, side: "right" } as Seg] : []),
    ...(f.model ? [{ id: "model", text: f.model, side: "right" } as Seg] : []),
    ...(f.usage ? [{ id: "usage", text: f.usage.text, side: "right" } as Seg] : []),
    ...(f.position ? [{ id: "position", text: f.position, side: "right" } as Seg] : []),
    ...(f.unread ? [{ id: "unread", text: f.unread, side: "right" } as Seg] : []),
  ];
  const width = (s: Seg[]) => s.reduce((n, x) => n + textWidth(x.text) + PAD, 0);
  const steps: ((s: Seg[]) => Seg[])[] = [
    // Usage drops before even shortening the context, so a narrow panel keeps the displayed file
    // name ahead of a token and cost figure.
    (s) => s.filter((x) => x.id !== "usage"),
    (s) => s.map((x) => (x.id === "context" && f.context ? { ...x, text: `⧉ ${f.context.file}` } : x)),
    (s) => s.filter((x) => x.id !== "model"),
    (s) => s.filter((x) => x.id !== "position"),
    (s) => s.filter((x) => x.id !== "context"),
    (s) => s.filter((x) => x.id !== "queue"),
    // #39: the Ctrl+y segment keeps the tool it names before it goes, and goes before the card
    // count -- `⚑N` is shorter, and still says a card waits.
    (s) => s.map((x) => (x.id === "approve" && f.approve ? { ...x, text: `Ctrl+y approves ${f.approve.tool}` } : x)),
    (s) => s.filter((x) => x.id !== "approve"),
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

/** `1234` -> `1.2k`, `1_200_000` -> `1.2M`: one decimal, and no trailing `.0`. A figure that would
 *  round up to `1000k` reads `1M` instead (`999_950` is the first such). */
function tokens(n: number): string {
  const scaled = (value: number, unit: string) => `${value.toFixed(1).replace(/\.0$/, "")}${unit}`;
  if (n < 1000) return `${n}`;
  return n < 999_950 ? scaled(n / 1e3, "k") : scaled(n / 1e6, "M");
}

/** Cents, except that a real cost below half a cent reads `<$0.01` rather than `$0.00`: a session that
 *  spent something never looks like one that spent nothing. */
function dollars(c: number): string {
  return c > 0 && c < 0.005 ? "<$0.01" : `$${c.toFixed(2)}`;
}

/** One formatter for the tooltip: `toLocaleString` builds a new `Intl.NumberFormat` on every call,
 *  and this runs on every render of the panel while a reply streams. */
const GROUPED = new Intl.NumberFormat("en-US");

/** Format the latest reported token counts and cost, without accumulating reports. Return null for
 *  unknown usage or a non-finite cost so rendering cannot throw. A legacy cost-only report shows
 *  only its cost. */
export function usageSegment(u: UsageInfo | null): UsageFact | null {
  if (u === null || !Number.isFinite(u.total_cost_usd)) return null;
  const t = u.tokens;
  const total = t === null ? null : t.input + t.output + t.cache_creation + t.cache_read;
  const text = total === null ? dollars(u.total_cost_usd) : `${tokens(total)} tok ${dollars(u.total_cost_usd)}`;
  const parts =
    t === null
      ? []
      : [
          `input ${GROUPED.format(t.input)}`,
          `output ${GROUPED.format(t.output)}`,
          `cache write ${GROUPED.format(t.cache_creation)}`,
          `cache read ${GROUPED.format(t.cache_read)}`,
        ];
  const what = "since this tab started or resumed; /clear starts it over; Claude Code's own estimate, not a bill";
  const title = `${[...parts, `$${u.total_cost_usd.toFixed(4)}`].join(" · ")} — ${what}`;
  return { text, title };
}
