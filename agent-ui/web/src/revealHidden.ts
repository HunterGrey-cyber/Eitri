/** Characters that change what a line looks like without looking like anything themselves, shown as
 *  visible escapes where a person decides whether to run what they read.
 *
 *  A permission card is the only view of the command being approved. Drawn raw, a right-to-left
 *  override can reorder what follows it, a zero-width space can split a word without showing a gap,
 *  and a no-break space looks like the space the shell would split at but is not one. So each such
 *  character is drawn as `⟨U+XXXX⟩` instead, and the bytes stay what they were.
 *
 *  What is escaped: control characters (`Cc`) except newline and tab, format characters (`Cf`: the
 *  bidi embeddings, overrides, isolates and marks, zero-width characters, the byte-order mark, the
 *  soft hyphen and the tag characters), lone surrogates, line and paragraph separators, every space
 *  but the plain one, and the characters that render blank: the combining grapheme joiner, the
 *  Hangul and Braille fillers, the Khmer inherent vowels, the Mongolian variation selectors and the
 *  variation selectors. Right-to-left letters are ordinary text and are left alone.
 */
export type RevealedPiece = { text: string } | { escape: string };

const HIDDEN =
  /[\p{Cc}\p{Cf}\p{Cs}\p{Zl}\p{Zp}\p{Zs}\u034F\u115F\u1160\u17B4\u17B5\u180B-\u180F\u2800\u3164\uFE00-\uFE0F\uFFA0\u{E0100}-\u{E01EF}]/u;

/** Kept as text: the card shows the command as the shell reads it, newlines and all. */
const KEPT = new Set(["\n", "\t", " "]);

function escapeOf(ch: string): string {
  const hex = ch.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0");
  return `⟨U+${hex}⟩`;
}

/** `text` as plain runs and escapes, in order; adjacent plain characters share one piece. */
export function revealHidden(text: string): RevealedPiece[] {
  const pieces: RevealedPiece[] = [];
  let run = "";
  // `for...of` walks code points, so a tag character or an astral variation selector is one
  // character, and a lone surrogate comes through on its own.
  for (const ch of text) {
    if (!KEPT.has(ch) && HIDDEN.test(ch)) {
      if (run !== "") {
        pieces.push({ text: run });
        run = "";
      }
      pieces.push({ escape: escapeOf(ch) });
    } else {
      run += ch;
    }
  }
  if (run !== "") pieces.push({ text: run });
  return pieces;
}

export function countEscapes(pieces: RevealedPiece[]): number {
  return pieces.filter((piece) => "escape" in piece).length;
}
