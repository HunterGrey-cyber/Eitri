import type { PanelMode } from "./keymap";
import type { EditorLink, UsageInfo } from "./types";

/** One segment's identity, in the priority order spec §5.2 lists (used only for lookups here --
 *  `bandLayout`'s own construction order, not this list, decides degrade order and render order). */
export type SegId = "mode" | "pill" | "showcmd" | "message" | "prompt" | "warn" | "unread" | "cards" | "approve" | "queue" | "context" | "link" | "position" | "model" | "usage";

/** One piece of the band: the text to show and which side of the gap it belongs on (spec §5.2:
 *  left of the gap is mode, pill, `⚑N`, `⧗N`, message; right of it is showcmd, `⚠`, context,
 *  model, usage, position, `↓N`). `StatusBand` groups by `side` when it renders; `bandLayout` itself
 *  never reorders by side, only by priority. */
export type Seg = {
  id: SegId;
  text: string;
  side: "left" | "right";
  /** Set only on `"prompt"`/`"message"`, and only when the FULL text (never truncated any more,
   *  defect 1 of the 2026-09-27 sandbox GUI pass) does not fit the band's one row at the measured
   *  width. `StatusBand` reads this to switch the band from its fixed one-editor-row height to a
   *  wrapping, auto-height one for as long as this text is shown -- a y/n prompt's own trailing
   *  `(y/n)`, or the D11 flash explaining what `y` does, used to be exactly the part `bandLayout`'s
   *  old `cut()` truncated away. Absent (not merely `false`) on every other segment, which keeps
   *  the existing drop-by-priority degrade instead. */
  wraps?: boolean;
};

/** What the band has to say this render, gathered by `App.tsx` from state that already exists
 *  elsewhere (this crate's own state, the tab's mode, the reducer's projection) -- `bandLayout` is
 *  a pure function over this plus the measured width, so the degrade rule (spec §5.3) is testable
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
  /** R5: the active tab's last reported usage, already worded (`usageSegment`) -- `text` is the
   *  segment, `title` its tooltip. `null` until a turn has reported one, and always `null` on the
   *  empty tab. */
  usage: UsageFact | null;
  /** Owner decision #39 (2026-09-30): the card INPUT's `Ctrl+y` would approve -- the active tab's
   *  oldest waiting card -- set by `App.tsx` only while one waits and the composer has the keys, so
   *  the band says which card that one key answers. Optional: absent (or `null`) is "nothing to
   *  name", which every caller that predates #39 means. */
  approve?: ApproveFact | null;
  /** Companion mode: where the panel stands with the editor beside it. `null`, or absent, is the
   *  one-window mode, which has no such thing and never sends one. While it is set and not
   *  `attached`, its text takes the place of the context segment: with no editor attached no file
   *  can be named. Optional so every caller that predates companion mode compiles unchanged. */
  link?: EditorLink | null;
};

/** #39's segment: the card's tool name and a one-line summary of its input (`cardSummary`). */
export type ApproveFact = { tool: string; summary: string };

/** How long `cardSummary` lets a card's summary run on the band, the `…` included. */
const CARD_SUMMARY_MAX = 40;

/** Owner decision #39: a card in one short line for the band -- what the call acts on (its command,
 *  file, path or URL, the fields a person checks before approving), else `summarizeInput`'s own
 *  order's remaining fields, else compact JSON; whitespace collapsed; cut to `CARD_SUMMARY_MAX`
 *  characters in the middle, head … tail (fix round 1). A `Bash` card's `description` is the
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
  // Fix round 1 (Opus M-3): cut in the middle, not at the end -- the tail of a long command is often
  // the part that matters (`| sudo bash`, `--force`), and it is what a person approving must see.
  const tail = Math.floor((CARD_SUMMARY_MAX - 1) / 2) + 4;
  const head = CARD_SUMMARY_MAX - 1 - tail;
  return `${chars.slice(0, head).join("")}…${chars.slice(chars.length - tail).join("")}`;
}

/** The band's usage segment as `usageSegment` words it: the text drawn, and the tooltip carrying
 *  the breakdown the text has no room for. */
export type UsageFact = { text: string; title: string };

/** vim's East Asian Wide/Fullwidth ranges, counted twice (spec §5.3, Review Focus 3): a CJK file
 *  name or model name must not silently overrun the band's monospace budget. Not a full Unicode
 *  East-Asian-Width table -- just the ranges the fixtures and the spec's own worked example
 *  (`说明文档非常长的文件名称.md`) actually exercise: CJK Unified Ideographs and their extension
 *  blocks, Hangul syllables and jamo, and the fullwidth forms block. */
const WIDE = /[ᄀ-ᅟ⺀-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]/u;

/** Character count with East Asian wide characters counted twice (spec §5.3). Iterates by code
 *  point, not UTF-16 unit, so a codepoint outside the BMP is never split into two "wide" halves. */
export function textWidth(text: string): number {
  let n = 0;
  for (const ch of text) n += WIDE.test(ch) ? 2 : 1;
  return n;
}

/** This module used to have a `cut(text, n)` truncator here, appending `…` to a message/prompt
 *  that did not fit (spec §5.3.3). **Removed 2026-09-27 (v1 sandbox GUI pass, defect 1):** at the
 *  owner's own WebKit zoom 1.5 (a 47-column band), it truncated the R06 bypass prompt (then in
 *  Chinese; English since K09, `Switch to bypass and approve the 1 waiting card? (y/n)`, which is
 *  wider than 47 columns too) mid-sentence, losing its own `(y/n)`, and did the same to the D11
 *  flash explaining what `y` does -- exactly the two things a y/n prompt/flash must stay legible
 *  for. Neither a prompt nor a message is ever cut any more; a segment that would have been reads
 *  `wraps: true` instead (below), and `StatusBand`/`index.css`'s `.status-band--wrap` let the band
 *  grow upward and wrap it onto more rows for as long as it is shown, returning to the fixed
 *  one-editor-row height the moment the prompt/flash ends. Ordinary segments (mode, pill, cards,
 *  queue, context, model, position, unread) are untouched: they still degrade by priority, below. */

const PAD = 2; // one character of padding each side (8px at --fs-sm)
// Visual-mode spec D16 (O7's kept default, revised for 3a): CARET / VISUAL / V-LINE (V-LINE's own
// word from lualine: utils/mode.lua:17,19, what his LazyVim statusline says; CARET has no lualine
// precedent -- Eitri-only, qutebrowser's own caret mode is the source, D16's own note), narrow
// C / V / VL.
const MODE_TEXT: Record<PanelMode, string> = { input: "INPUT", browse: "BROWSE", hint: "HINT", caret: "CARET", visual: "VISUAL", vline: "V-LINE" };
const MODE_LETTER: Record<PanelMode, string> = { input: "I", browse: "B", hint: "H", caret: "C", visual: "V", vline: "VL" };

/** The bottom band's degrade rule (spec §5.3), a pure function of the facts and the measured
 *  width so it is testable without a browser. `widthPx <= 0` (before the first measurement,
 *  Review Focus 3) or `charPx <= 0` (the hidden measuring span not yet laid out) shows only mode
 *  and pill -- priorities 1 and 2's own floor, never dropped once a real width is known either. */
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
  const link = f.link && f.link.state !== "attached" && f.link.text !== "" ? f.link : null;
  let segs: Seg[] = [
    mode,
    pill,
    ...(f.cards > 0 ? [{ id: "cards", text: `⚑${f.cards}`, side: "left" } as Seg] : []),
    ...(f.approve ? [{ id: "approve", text: `Ctrl+y approves ${f.approve.tool}: ${f.approve.summary}`, side: "left" } as Seg] : []),
    ...(f.queued > 0 ? [{ id: "queue", text: `⧗${f.queued}`, side: "left" } as Seg] : []),
    ...(f.message ? [{ id: "message", text: f.message, side: "left" } as Seg] : []),
    ...(f.showcmd ? [{ id: "showcmd", text: f.showcmd, side: "right" } as Seg] : []),
    ...(f.warn ? [{ id: "warn", text: "⚠", side: "right" } as Seg] : []),
    ...(link ? [{ id: "link", text: link.text, side: "right" } as Seg] : []),
    ...(ctxFull && !link ? [{ id: "context", text: ctxFull, side: "right" } as Seg] : []),
    ...(f.model ? [{ id: "model", text: f.model, side: "right" } as Seg] : []),
    ...(f.usage ? [{ id: "usage", text: f.usage.text, side: "right" } as Seg] : []),
    ...(f.position ? [{ id: "position", text: f.position, side: "right" } as Seg] : []),
    ...(f.unread ? [{ id: "unread", text: f.unread, side: "right" } as Seg] : []),
  ];
  const width = (s: Seg[]) => s.reduce((n, x) => n + textWidth(x.text) + PAD, 0);
  const steps: ((s: Seg[]) => Seg[])[] = [
    // R5: the usage figure is the least urgent thing on the band -- it goes before even the context
    // is shortened, so a narrow panel never trades the file it is showing for a running total.
    (s) => s.filter((x) => x.id !== "usage"),
    (s) => s.map((x) => (x.id === "context" && f.context ? { ...x, text: `⧉ ${f.context.file}` } : x)),
    (s) => s.filter((x) => x.id !== "model"),
    (s) => s.filter((x) => x.id !== "position"),
    (s) => s.filter((x) => x.id !== "context" && x.id !== "link"),
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
  // Defect 1: `message` is never cut either (`over > 0` here means every droppable segment is
  // already gone and the message's own full text is still wider than the row) -- it is marked
  // `wraps` instead of being shortened, so `StatusBand` grows the band rather than hiding the end
  // of a flash like the D11 explanation.
  const over = width(segs) - budget;
  if (over > 0) segs = segs.map((x) => (x.id === "message" ? { ...x, wraps: true } : x));
  return segs;
}

/** The model without its leading `claude-` (spec §5.2's model row): `"claude-sonnet-5"` ->
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

/** R5: every token the provider counted (the prompt's three parts and the output) and the cost -- the
 *  latest report, never a sum; `null` until one arrives, never `0 tok $0.00` for unknown. A backend that
 *  reports a cost and no tokens (legacy) shows the cost alone. A cost that is not a finite number is no
 *  measurement either (Rust drops such a report whole, `usage_from_proto`; this keeps one that slipped
 *  through from throwing inside a render). */
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
