import type MarkdownIt from "markdown-it";
import { mark, NONCE_ATTR } from "./env";

/* A line as written, without its line ending and trailing blanks. */
function rawLine(src: string, begin: number, end: number): string {
  return src.slice(begin, end).replace(/[ \t]+$/, "");
}

/** YAML front matter: line 1 exactly `---` at column 0, closed by a later line that is exactly `---` or `...`.
 *  Anything else is ordinary Markdown, so a document that opens with a thematic break and never closes it keeps its
 *  rule and its text. It renders as a closed `<details>` titled "Front matter" holding the YAML as code, and its token
 *  covers every line of the block, so the one-line summary stands for all of them. */
export function frontMatterPlugin(
  md: MarkdownIt,
  highlight: (code: string, lang: string, nonce: string) => string | null,
): void {
  md.block.ruler.before("table", "front_matter", (state, startLine, endLine, silent) => {
    if (startLine !== 0 || state.blkIndent !== 0) return false;
    if (rawLine(state.src, state.bMarks[0], state.eMarks[0]) !== "---") return false;
    let close = -1;
    for (let line = 1; line < endLine; line++) {
      const text = rawLine(state.src, state.bMarks[line], state.eMarks[line]);
      if (text === "---" || text === "...") {
        close = line;
        break;
      }
    }
    if (close < 0) return false;
    if (silent) return true;
    const token = state.push("front_matter", "details", 0);
    token.block = true;
    token.markup = "---";
    token.map = [0, close + 1];
    token.content = state.getLines(1, close, 0, false);
    state.line = close + 1;
    return true;
  });

  md.renderer.rules.front_matter = (tokens, idx, _options, env, slf) => {
    const token = tokens[idx];
    const nonce = (env as { nonce: string }).nonce;
    token.attrJoin("class", "front-matter");
    token.attrSet(NONCE_ATTR, nonce);
    const code = highlight(token.content, "yaml", nonce) ?? md.utils.escapeHtml(token.content);
    const n = mark({ nonce });
    return `<details${slf.renderAttrs(token)}><summary${n}>Front matter</summary>` +
      `<pre${n}><code class="hljs language-yaml"${n}>${code}</code></pre></details>\n`;
  };
}
