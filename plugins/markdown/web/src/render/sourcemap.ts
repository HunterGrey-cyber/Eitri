import type MarkdownIt from "markdown-it";
import { NONCE_ATTR } from "./env";

/* Block tokens that render a whole element of their own with no open/close pair, so the opening-token test below
 * would miss them. `html_block` is deliberately absent: its HTML is the document's own, and an attribute set on its
 * token would go nowhere; the engine writes an empty marker element in front of it instead. */
const LEAVES = new Set(["fence", "code_block", "hr", "math_block", "front_matter"]);

/** Tags every block the document's lines map to with those lines, so the page can find the element for a line and
 *  the line for an element: `data-line` (first line, 0-based), `data-line-end` (one past the last), the class
 *  `code-line` the sync code looks for, and the render's nonce so the sanitiser keeps all three. It runs after every
 *  other core rule, so the tokens it sees are the final ones (alerts retagged, footnotes moved to the end). Inline
 *  tokens carry no lines and are never touched; a hidden token (a tight list's paragraph) renders nothing to tag. */
export function sourceMapPlugin(md: MarkdownIt): void {
  md.core.ruler.push("eitri_source_map", (state) => {
    const nonce = (state.env as { nonce: string }).nonce;
    for (const token of state.tokens) {
      if (!token.map || token.hidden) continue;
      if (token.nesting === 1 || (token.nesting === 0 && LEAVES.has(token.type))) {
        token.attrSet("data-line", String(token.map[0]));
        token.attrSet("data-line-end", String(token.map[1]));
        token.attrJoin("class", "code-line");
        token.attrSet(NONCE_ATTR, nonce);
      }
    }
    return false;
  });
}
