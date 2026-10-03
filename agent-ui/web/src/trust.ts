import { isPlainAnswerKey } from "./keymap";
import type { KeyLike } from "./keymap";
import { bypassYesCounts } from "./modeKey";
import type { TabId } from "./types";

/** One thing the project's own Claude configuration would run or load, as Rust found it. Every string is
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

/** Files that differ from what was trusted before, relative to the repository top. */
export type TrustChanged = { added: string[]; removed: string[]; changed: string[] };

/** How long an answer is remembered: `yes` is a lasting record, `window` lasts until this window closes,
 *  `session` covers this one start only (something could not be checked). */
export type TrustRemember = "yes" | "window" | "session";

/** The `trust_prompt` envelope (`serialize_trust_prompt_for_js`, `core/src/agent_bridge.rs`). `fingerprint`
 *  and `findingsDigest` are echoed back verbatim by an answer and never recomputed here. */
export type TrustPromptEnvelope = {
  kind: "trust_prompt";
  tab: TabId;
  nonce: number;
  root: string;
  top: string;
  fingerprint: string;
  findingsDigest: string;
  state: "untrusted" | "changed";
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

/** The overlay's last line, and the flash for a key the prompt does not take. A `session` prompt says what
 *  its `y` is worth: the start in front of it and nothing after. */
export function trustFooter(remember: TrustRemember): string {
  const base = "y trusts and loads it · n starts without it · Esc puts this off";
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
  ctx: { now: number; openedAt: number; lastKeyAt: number; pendingG: boolean },
): TrustKeyAction {
  const plain = isPlainAnswerKey(event) && event.altKey !== true && event.metaKey !== true;
  if (event.key === "Escape") return { kind: "cancel" };
  const lower = event.key.length === 1 ? event.key.toLowerCase() : event.key;
  if ((lower === "y" || lower === "n") && !event.ctrlKey) {
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

/** What the `:` line does with its text: the exact words only, so `:trust x` and `:trusted` stay a refusal. */
export function parseTrustCommand(line: string): "trust" | "untrust" | null {
  const word = line.trim();
  return word === "trust" || word === "untrust" ? word : null;
}
