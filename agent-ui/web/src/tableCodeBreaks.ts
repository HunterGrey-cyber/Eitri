/* Code in a table cell keeps its words whole (`index.css`: no `overflow-wrap` there), so a column holding a long path
   cannot be narrower than that whole path, and the columns beside it are squeezed to nothing. A path does have places
   it can break without changing what it says: after a `/`, and after a `,` between the alternatives of a `{a,b}`.
   Those get a `<wbr>`, which has no text of its own, so what is copied is what was written. The column's smallest
   width becomes its longest segment. */

const CODE_IN_CELL = "td code, th code";

/** Where a break may go in `text`, as the index each `<wbr>` goes before, and the brace depth it ends at. A run of
 *  slashes stays together (`https://`), nothing breaks at the end of the text (no `<wbr>` after the last character),
 *  and a comma breaks only while a `{` is still open. `depth` carries over from the piece of the same code before. */
export function breakPointsIn(text: string, depth: number): { points: number[]; depth: number } {
  const points: number[] = [];
  for (let i = 0; i < text.length - 1; i++) {
    const ch = text[i];
    if (ch === "{") depth += 1;
    else if (ch === "}") depth = Math.max(0, depth - 1);
    else if ((ch === "/" && text[i + 1] !== "/") || (ch === "," && depth > 0)) points.push(i + 1);
  }
  const last = text[text.length - 1];
  if (last === "{") depth += 1;
  else if (last === "}") depth = Math.max(0, depth - 1);
  return { points, depth };
}

/** Puts the breaks into every inline code span inside a table cell of `root` (sanitized already; only `<wbr>` elements
 *  are added, between and never inside the text a person reads, and an outlined escape is left whole). */
export function breakTableCode(root: ParentNode): void {
  for (const code of root.querySelectorAll(CODE_IN_CELL)) {
    if (code.closest("pre") !== null) continue;
    const walker = document.createTreeWalker(code, NodeFilter.SHOW_TEXT);
    const texts: Text[] = [];
    while (walker.nextNode()) {
      const node = walker.currentNode as Text;
      if (node.parentElement?.closest(".code-escape") == null) texts.push(node);
    }
    let depth = 0;
    for (const node of texts) {
      const found = breakPointsIn(node.data, depth);
      depth = found.depth;
      // From the back, so an earlier index is still where it was after a later split.
      for (const at of found.points.reverse()) {
        const tail = node.splitText(at);
        tail.before(document.createElement("wbr"));
      }
    }
  }
}
