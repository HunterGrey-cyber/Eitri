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

/** The runs of two or more line breaks in a node, as the offsets each starts at and ends before, in order. */
interface Runs { starts: number[]; ends: number[] }
const runs = new WeakMap<Text, { data: string; at: Runs }>();

/** The runs of line breaks of `t` (whose data is `data`), found once per value of its data with one pattern search,
 *  so a code block's blank lines are passed by halving over them, never by a walk over each line break. */
export function breakRuns(t: Text, data: string = t.data): Runs {
  const hit = runs.get(t);
  if (hit && hit.data === data) return hit.at;
  const at: Runs = { starts: [], ends: [] };
  for (const m of data.matchAll(/\n{2,}/g)) {
    at.starts.push(m.index);
    at.ends.push(m.index + m[0].length);
  }
  runs.set(t, { data, at });
  return at;
}

/* How the letters of the joining scripts (Arabic, Syriac, N'Ko, Mongolian, Adlam and their relatives) connect to their
   neighbours: Unicode's Joining_Type (ArabicShaping.txt, Unicode 15.0), as hexadecimal ranges with the type after
   each, for D (dual-joining: connects on both sides), R (right-joining: only to the letter before it in the text), L
   (only to the letter after it) and C (join-causing: the tatweel, the zero-width joiner). Every other character is
   transparent (a mark or a format character, `Mn` `Me` `Cf`, which a joining letter connects across) or does not
   join. */
const JOINING =
  "620D 622-625R 626D 627R 628D 629R 62a-62eD 62f-632R 633-63fD 640C 641-647D 648R 649-64aD 66e-66fD " +
  "671-673R 675-677R 678-687D 688-699R 69a-6bfD 6c0R 6c1-6c2D 6c3-6cbR 6ccD 6cdR 6ceD 6cfR 6d0-6d1D " +
  "6d2-6d3R 6d5R 6ee-6efR 6fa-6fcD 6ffD 710R 712-714D 715-719R 71a-71dD 71eR 71f-727D 728R 729D 72aR 72bD " +
  "72cR 72d-72eD 72fR 74dR 74e-758D 759-75bR 75c-76aD 76b-76cR 76d-770D 771R 772D 773-774R 775-777D " +
  "778-779R 77a-77fD 7ca-7eaD 7faC 840R 841-845D 846-847R 848D 849R 84a-853D 854R 855D 856-858R 860D " +
  "862-865D 867R 868D 869-86aR 870-882R 883-885C 886D 889-88dD 88eR 8a0-8a9D 8aa-8acR 8aeR 8af-8b0D " +
  "8b1-8b2R 8b3-8b8D 8b9R 8ba-8c8D 1807D 180aC 1820-1878D 1887-18a8D 18aaD 200dC a840-a871D a872L " +
  "10ac0-10ac4D 10ac5R 10ac7R 10ac9-10acaR 10acdL 10ace-10ad2R 10ad3-10ad6D 10ad7L 10ad8-10adcD 10addR " +
  "10ade-10ae0D 10ae1R 10ae4R 10aeb-10aeeD 10aefR 10b80D 10b81R 10b82D 10b83-10b85R 10b86-10b88D 10b89R " +
  "10b8a-10b8bD 10b8cR 10b8dD 10b8e-10b8fR 10b90D 10b91R 10ba9-10bacR 10bad-10baeD 10d00L 10d01-10d21D " +
  "10d22R 10d23D 10f30-10f32D 10f33R 10f34-10f44D 10f51-10f53D 10f54R 10f70-10f73D 10f74-10f75R " +
  "10f76-10f81D 10fb0D 10fb2-10fb3D 10fb4-10fb6R 10fb8D 10fb9-10fbaR 10fbb-10fbcD 10fbdR 10fbe-10fbfD " +
  "10fc1D 10fc2-10fc3R 10fc4D 10fc9R 10fcaD 10fcbL 1e900-1e943D";
const ZWJ = "\u200d";
const TRANSPARENT = /^[\p{Mn}\p{Me}\p{Cf}]$/u;
let joining: { from: number[]; to: number[]; type: string } | null = null;

/** The joining type of the code point `cp`: one of `D` `R` `L` `C`, `T` (transparent) or `U` (does not join). */
function joiningType(cp: number): string {
  if (!joining) {
    joining = { from: [], to: [], type: "" };
    for (const entry of JOINING.split(" ")) {
      const [, a, b, type] = /^([0-9a-f]+)(?:-([0-9a-f]+))?([DRLC])$/.exec(entry)!;
      joining.from.push(parseInt(a, 16));
      joining.to.push(parseInt(b ?? a, 16));
      joining.type += type;
    }
  }
  const k = lowerIndex(joining.to, cp);
  if (k < joining.to.length && joining.from[k] <= cp) return joining.type[k];
  return cp !== 0x200c && TRANSPARENT.test(String.fromCodePoint(cp)) ? "T" : "U";
}

/** The joining type of the first character before (`dir` -1) or from (`dir` 1) offset `i` of `data` that is not
 *  transparent, or `U` past either end. */
function neighbour(data: string, i: number, dir: 1 | -1): string {
  for (let o = i; o >= 0 && o < data.length;) {
    // Read backward, a surrogate pair is met at its second half.
    if (dir < 0 && o > 0 && /[\udc00-\udfff]/.test(data[o])) o--;
    const cp = data.codePointAt(o) ?? 0;
    const type = joiningType(cp);
    if (type !== "T") return type;
    o = dir > 0 ? o + (cp > 0xffff ? 2 : 1) : o - 1;
  }
  return "U";
}

/** The character of `data` at [`at`, `at + len`) as the block draws it alone, with a zero-width joiner on each side
 *  where it connects to its neighbour in the text, so a letter of a joining script keeps the form the text gives it
 *  (the medial `ت` of `بتب`, not its isolated form). Its neighbours are read in the same text, so a letter whose
 *  word goes on in another element (a highlight, an emphasis) is drawn as that side does not connect. */
export function joinedGlyph(data: string, at: number, len: number): string {
  const ch = data.slice(at, at + len);
  const own = joiningType(ch.codePointAt(0) ?? 0);
  if (own === "U" || own === "T") return ch;
  const before = (own === "D" || own === "R" || own === "C") && /[DLC]/.test(neighbour(data, at - 1, -1));
  const after = (own === "D" || own === "L" || own === "C") && /[DRC]/.test(neighbour(data, at + len, 1));
  return `${before ? ZWJ : ""}${ch}${after ? ZWJ : ""}`;
}
