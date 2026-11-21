/* What the block cursor treats as one character: a grapheme cluster (a letter with its combining marks, an emoji
   with its joiners and modifiers, a flag), the unit a reader sees. A text node is segmented once per value of its
   data; a browser without `Intl.Segmenter` steps by code point (a surrogate pair is one). Clusters that straddle two
   text nodes are two characters: no node reads past its own data. */

/* The page's library target predates `Intl.Segmenter`'s types; the engines it runs in (WebKitGTK, WKWebView) have it. */
interface Segmenter { segment(text: string): Iterable<{ index: number }> }
const Ctor = typeof Intl === "undefined" ? undefined
  : (Intl as unknown as { Segmenter?: new (locale: undefined, o: { granularity: "grapheme" }) => Segmenter }).Segmenter;
const segmenter: Segmenter | null = Ctor ? new Ctor(undefined, { granularity: "grapheme" }) : null;

/** Text that is one character per code unit and has no cluster to find: printable ASCII, tab and line feed. */
const PLAIN = /^[\x20-\x7e\t\n]*$/;

/** The offsets at which a character of the node starts, or null when every code unit is one. */
const known = new WeakMap<Text, { data: string; starts: number[] | null }>();

function startsOf(t: Text): number[] | null {
  const data = t.data;
  const hit = known.get(t);
  if (hit && hit.data === data) return hit.starts;
  let starts: number[] | null = null;
  if (!PLAIN.test(data)) {
    starts = [];
    if (segmenter) for (const s of segmenter.segment(data)) starts.push(s.index);
    else for (let i = 0; i < data.length; i += isPair(data, i) ? 2 : 1) starts.push(i);
  }
  known.set(t, { data, starts });
  return starts;
}

function isPair(data: string, i: number): boolean {
  const high = data.charCodeAt(i);
  const low = data.charCodeAt(i + 1);
  return high >= 0xd800 && high < 0xdc00 && low >= 0xdc00 && low < 0xe000;
}

/** The first index of `starts` that is greater than `i`. */
function after(starts: number[], i: number): number {
  let lo = 0;
  let hi = starts.length;
  while (lo < hi) {
    const m = (lo + hi) >> 1;
    if (starts[m] > i) hi = m;
    else lo = m + 1;
  }
  return lo;
}

/** How many code units the character that starts at `i` takes. */
export function charLength(t: Text, i: number): number {
  const starts = startsOf(t);
  if (!starts) return 1;
  const next = after(starts, i);
  return (next < starts.length ? starts[next] : t.data.length) - i;
}

/** `i`, or the start of the character it falls inside. */
export function charStart(t: Text, i: number): number {
  const starts = startsOf(t);
  if (!starts) return i;
  const next = after(starts, i);
  return next > 0 ? starts[next - 1] : i;
}

/** How many characters start in [`a`, `b`) of `t` (`a` and `b` at character starts or the node's end): found by
 *  halving over the node's character starts, so a count across a long node is not a walk over it. */
export function charsBetween(t: Text, a: number, b: number): number {
  if (b <= a) return 0;
  const starts = startsOf(t);
  return starts ? after(starts, b - 1) - after(starts, a - 1) : b - a;
}

/** The characters of a node that may draw nothing, as the offsets they start at, in order: `blank` those that begin
 *  with a space or a tab, `other` those that begin with any other white space, a control or a format character. */
interface Odd { blank: number[]; other: number[] }
const odd = new WeakMap<Text, { data: string; at: Odd }>();
const BLANK = /[ \t]/;
const OTHER = /[\s\p{C}]/u;

/** The characters of `t` that may draw nothing (`Odd`), read once per value of its data. */
export function oddsOf(t: Text): Odd {
  const data = t.data;
  const hit = odd.get(t);
  if (hit && hit.data === data) return hit.at;
  const at: Odd = { blank: [], other: [] };
  const starts = startsOf(t);
  const visit = (i: number) => {
    const ch = String.fromCodePoint(data.codePointAt(i) ?? 0);
    if (BLANK.test(ch)) at.blank.push(i);
    else if (OTHER.test(ch)) at.other.push(i);
  };
  if (starts) for (const i of starts) visit(i);
  else for (let i = 0; i < data.length; i++) visit(i);
  odd.set(t, { data, at });
  return at;
}

/** The first index of the ascending `list` whose value is at least `v`. */
export function lowerIndex(list: number[], v: number): number {
  let lo = 0;
  let hi = list.length;
  while (lo < hi) {
    const m = (lo + hi) >> 1;
    if (list[m] < v) lo = m + 1;
    else hi = m;
  }
  return lo;
}

/** Characters whose lines run right to left: Hebrew, Arabic and their relatives, and the directional marks. */
const RTL = /[֐-ࣿ‏‫‮⁧יִ-﷿ﹰ-ﻼ]|[\u{10800}-\u{10fff}\u{1e800}-\u{1efff}]/u;

/** Whether `text` holds a character of a right-to-left script. */
export function hasRtl(text: string): boolean { return RTL.test(text); }
