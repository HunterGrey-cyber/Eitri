import MarkdownIt from "markdown-it";
import { describe, expect, it } from "vitest";
import { sourceMapPlugin } from "./sourcemap";

describe("sourceMapPlugin", () => {
  it("tags opening tokens and the leaf blocks it knows, never inline tokens or hidden paragraphs", () => {
    const md = new MarkdownIt();
    sourceMapPlugin(md);
    // A list and its last item run to the blank line after them, as markdown-it maps them.
    const tokens = md.parse("- a\n- b\n\npara\n\n    code\n", { nonce: "N" });
    const tagged = tokens.filter((t) => t.attrGet("data-line") !== null).map((t) => [t.type, t.attrGet("data-line"),
      t.attrGet("data-line-end"), t.attrGet("class"), t.attrGet("data-eitri-n")]);
    expect(tagged).toEqual([
      ["bullet_list_open", "0", "3", "code-line", "N"],
      ["list_item_open", "0", "1", "code-line", "N"],
      ["list_item_open", "1", "3", "code-line", "N"],
      ["paragraph_open", "3", "4", "code-line", "N"],
      ["code_block", "5", "6", "code-line", "N"],
    ]);
  });
});
