import MarkdownIt from "markdown-it";
import footnote from "markdown-it-footnote";
import taskLists from "@hedgedoc/markdown-it-task-lists";
import alerts from "markdown-it-github-alerts";
import katexPluginModule from "@vscode/markdown-it-katex";
import GithubSlugger from "github-slugger";
import morphdom from "morphdom";
import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/core";
import katex from "katex";
import { describe, expect, it } from "vitest";
import { cjsDefault } from "./render/interop";

describe("the runtime packages", () => {
  it("load as the page modules use them", () => {
    const katexPlugin = cjsDefault<(md: MarkdownIt, o?: object) => void>(katexPluginModule);
    expect(typeof katexPlugin).toBe("function");
    const md = new MarkdownIt({ html: true }).use(footnote).use(taskLists, { enabled: false, label: false, lineNumber: false })
      .use(alerts).use(katexPlugin);
    const html = md.render("- [x] done\n\n> [!NOTE]\n> n\n\nx[^1] $a$\n\n[^1]: f\n");
    expect(html).toContain('type="checkbox"');
    expect(html).toContain("markdown-alert-note");
    expect(html).toContain("footnote-ref");
    expect(html).toContain("katex");
    expect(new GithubSlugger().slug("Hello World")).toBe("hello-world");
    expect(typeof morphdom).toBe("function");
    expect(typeof DOMPurify(window).sanitize).toBe("function");
    expect(typeof hljs.highlight).toBe("function");
    expect(katex.renderToString("x")).toContain("katex");
  });
});
