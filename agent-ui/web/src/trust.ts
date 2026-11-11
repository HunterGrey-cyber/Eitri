import type { DiffLine } from "./diff";
import { isPlainAnswerKey } from "./keymap";
import type { KeyLike } from "./keymap";
import { bypassYesCounts } from "./modeKey";
import type { TabId } from "./types";

/** One thing the project's own configuration for the tab's agent would run or load, as Rust found it. Every string is
 *  text to draw as a text node: a hook command or a file name is attacker-controlled. */
export type TrustItem = {
  /** `hook`, `mcp_server`, `symlink`, `unreadable`, ... -- the kind Rust grouped it under. */
  what: string;
  /** The file it was found in, relative to the top of the repository. */
  file: string;
  label: string;
  value: string;
  /** A link whose target lies outside the repository. */
  outside: boolean;
};

/** A bounded instruction-file change prepared by the worker. Every string is repository text,
 *  drawn as a text node with hidden characters revealed. */
export type TrustDiffLine = DiffLine;
export type TrustHunk = { skipped: number; lines: TrustDiffLine[] };
export type TrustText =
  | { kind: "diff"; path: string; added: number; removed: number; skippedAfter: number; hunks: TrustHunk[] }
  | { kind: "plain"; path: string; line: string };
/** Files that differ from what was trusted before, relative to the repository top. */
export type TrustChanged = { added: string[]; removed: string[]; changed: string[]; texts?: TrustText[] };

/** How long an answer is remembered: `yes` is a lasting record, `window` lasts until this window closes,
 *  `session` covers this one start only (something could not be checked). */
export type TrustRemember = "yes" | "window" | "session";

/** The `trust_prompt` envelope (`serialize_trust_prompt_for_js`, `core/src/agent_bridge.rs`). `fingerprint`
 *  and `findingsDigest` are echoed back verbatim by an answer and never recomputed here. */
export type TrustPromptEnvelope = {
  kind: "trust_prompt";
  tab: TabId;
  nonce: number;
  /** Whose configuration the question is about, as a person reads it: `Claude`, `Codex`, or a registered
   *  agent's own name. */
  agent: string;
  root: string;
  top: string;
  fingerprint: string;
  findingsDigest: string;
  /** `nothing`: none of the agent's files are in the project, and the agent is asked about anyway (it loads
   *  whatever project files appear, so it starts only after a `y`). `too_large`: the configuration is too large
   *  to show whole, so nothing is asked to be trusted: the question takes `n` and `Esc` and never a `y`. */
  state: "untrusted" | "changed" | "nothing" | "too_large";
  /** Whether `n` refuses the start instead of starting without the project's configuration: a registered ACP
   *  agent has no way to leave it out. Absent reads as `false`. */
  needsYes?: boolean;
  remember: TrustRemember;
  rememberNote: string | null;
  changed: TrustChanged | null;
  items: TrustItem[];
};

/** What Rust sends is checked once, here and in `bridge.ts`, so the drawing code can rely on the shape. */
const HEX64 = /^[0-9a-f]{64}$/;
export function isHex64(value: unknown): value is string {
  return typeof value === "string" && HEX64.test(value);
}
export function isTrustRemember(value: unknown): value is TrustRemember {
  return value === "yes" || value === "window" || value === "session";
}

/** The items sharing a file, in the order the files first appear. Rust's order inside a file is kept. */
export function groupItemsByFile(items: readonly TrustItem[]): { file: string; items: TrustItem[] }[] {
  const groups: { file: string; items: TrustItem[] }[] = [];
  const at = new Map<string, number>();
  for (const item of items) {
    const found = at.get(item.file);
    if (found === undefined) {
      at.set(item.file, groups.length);
      groups.push({ file: item.file, items: [item] });
    } else {
      groups[found].items.push(item);
    }
  }
  return groups;
}

/** The band's prompt while the question is up. */
export const TRUST_BAND_PROMPT = "trust? y/n";
/** The band while the question for a configuration too large to show is up: it takes no `y`. */
export const TRUST_BAND_PROMPT_N_ONLY = "trust? n/Esc";

/** The overlay's last line, and the flash for a key the prompt does not take. A `session` prompt says what
 *  its `y` is worth: the start in front of it and nothing after. */
export function trustFooter(remember: TrustRemember, needsYes = false, state?: TrustPromptEnvelope["state"]): string {
  if (state === "too_large") return "n starts without it · Esc puts this off · y is not taken here";
  const base = needsYes
    ? "y trusts and starts it · n does not start it · Esc puts this off"
    : "y trusts and loads it · n starts without it · Esc puts this off";
  return remember === "session" ? `${base} · y trusts this start only` : base;
}

/** The flash for a `y`/`n` that did not count (too soon, repeated, or typed with a modifier). */
export const TRUST_WAIT_FLASH = "wait a moment, then y or n";

export type TrustScroll = "line-down" | "line-up" | "half-down" | "half-up" | "top" | "bottom";

/** What one key does to the open prompt. */
export type TrustKeyAction =
  | { kind: "answer"; trust: boolean }
  | { kind: "cancel" }
  | { kind: "scroll"; by: TrustScroll }
  /** The first `g` of `gg`: the caller remembers it for exactly the next key. */
  | { kind: "pending_g" }
  /** A `y` or `n` that does not count: no plain key, a repeat, or inside the typing guard. */
  | { kind: "wait" }
  /** Anything else: taken, never passed on, and answered with the footer's text. */
  | { kind: "swallow" };

export type TrustKeyEvent = KeyLike & { repeat: boolean };

/** The whole key table of the prompt, pure and clock-injected. `y` and `n` count only as a plain key, not
 *  held, and past the same 250 ms wait the bypass prompt's `y` needs (since the prompt appeared and since
 *  the last key anywhere in the panel): a letter of prose typed while the question came up must not load
 *  a repository's hooks. `Escape` starts nothing, so it has no guard. */
export function resolveTrustKey(
  event: TrustKeyEvent,
  ctx: { now: number; openedAt: number; lastKeyAt: number; pendingG: boolean; onlyN?: boolean },
): TrustKeyAction {
  const plain = isPlainAnswerKey(event) && event.altKey !== true && event.metaKey !== true;
  if (event.key === "Escape") return { kind: "cancel" };
  const lower = event.key.length === 1 ? event.key.toLowerCase() : event.key;
  if ((lower === "y" || lower === "n") && !event.ctrlKey) {
    // The question for a configuration too large to show has no `y`: it is taken and answered with the
    // footer, never sent.
    if (lower === "y" && ctx.onlyN === true) return { kind: "swallow" };
    const counts =
      plain &&
      !event.repeat &&
      bypassYesCounts({ now: ctx.now, openedAt: ctx.openedAt, lastKeyAt: ctx.lastKeyAt });
    return counts ? { kind: "answer", trust: lower === "y" } : { kind: "wait" };
  }
  if (event.altKey === true || event.metaKey === true || event.isComposing) return { kind: "swallow" };
  if (event.ctrlKey) {
    if (event.key === "d") return { kind: "scroll", by: "half-down" };
    if (event.key === "u") return { kind: "scroll", by: "half-up" };
    return { kind: "swallow" };
  }
  if (event.key === "j") return { kind: "scroll", by: "line-down" };
  if (event.key === "k") return { kind: "scroll", by: "line-up" };
  if (event.key === "G") return { kind: "scroll", by: "bottom" };
  if (event.key === "g") return ctx.pendingG ? { kind: "scroll", by: "top" } : { kind: "pending_g" };
  return { kind: "swallow" };
}

/** Where a scroll key moves the overlay's own scroll box, the clamp included. `step` is what `j`/`k` move
 *  by, measured by the caller from the real line height; a half page is the box's own height over two. */
export function scrollTarget(
  by: TrustScroll,
  box: { scrollTop: number; scrollHeight: number; clientHeight: number },
  step: number,
): number {
  const page = Math.max(step, box.clientHeight / 2);
  const max = Math.max(0, box.scrollHeight - box.clientHeight);
  const wanted =
    by === "line-down" ? box.scrollTop + step
    : by === "line-up" ? box.scrollTop - step
    : by === "half-down" ? box.scrollTop + page
    : by === "half-up" ? box.scrollTop - page
    : by === "top" ? 0
    : max;
  return Math.min(max, Math.max(0, wanted));
}

/** An ex command the `:` line runs. `tabnew` carries the agent word typed after it, `null` for none. */
export type ExCommand =
  | { command: "history"; action: "older" | "latest" }
  | { command: "trust" | "untrust" }
  | { command: "tabnew"; agent: string | null }
  | { command: "scratch"; whole: boolean };

/** What the `:` line does with its text: the exact words only, so `:trust x` and `:trusted` stay a
 *  refusal. `:tabnew` takes at most one word, which is not checked here: Rust knows which agents `init.lua`
 *  registered, and its refusal names every accepted word. */
export function parseExCommand(line: string): ExCommand | null {
  const [head, argument, ...rest] = line.trim().split(/\s+/);
  if (head === "history" && rest.length === 0 && (argument === "older" || argument === "latest")) return { command: "history", action: argument };
  if (head === "trust" || head === "untrust") return argument === undefined ? { command: head } : null;
  if (head === "scratch" || head === "%scratch") return argument === undefined ? { command: "scratch", whole: head === "%scratch" } : null;
  if (head === "tabnew" && rest.length === 0) return { command: "tabnew", agent: argument ?? null };
  return null;
}
