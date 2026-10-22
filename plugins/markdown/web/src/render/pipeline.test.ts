import { afterEach, describe, expect, it, vi } from "vitest";
import { createEngine } from "./engine";
import { createMermaid, type MermaidRunner } from "./mermaid";
import { renderDocument } from "./pipeline";
import { yankText } from "../view/yank";

const DOC = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/" };
const SETTINGS = { auto: false, remote_images: "https" as const, sync: "both" as const };
const body = (text: string) => ({ buf: 3, tick: 41, name: "/p/README.md", text });

afterEach(() => vi.restoreAllMocks());

describe("renderDocument", () => {
  it("crlf_bom_and_replacement_characters_keep_source_lines", () => {
    const text = "\uFEFF# T\r\n\r\npara one\r\n\r\n\uFFFD bad line\r\n\r\nlast";
    const result = renderDocument(createEngine(), body(text), DOC, SETTINGS, createMermaid());
    const host = document.createElement("div");
    host.append(result.fragment);
    expect(host.querySelector("h1")!.getAttribute("data-line")).toBe("0");
    expect([...host.querySelectorAll("p")].map((p) => [p.getAttribute("data-line"), p.textContent]))
      .toEqual([["2", "para one"], ["4", "\uFFFD bad line"], ["6", "last"]]);
    expect(host.lastElementChild!.getAttribute("data-line")).toBe("7");
    expect(result.lines.length).toBe(7);
    expect(yankText(result.lines, { line: 2, end: 3, fence: false })).toBe("para one");
    expect(yankText(result.lines, { line: 4, end: 5, fence: false })).toBe("\uFFFD bad line");
  });

  it("returns a sanitised fragment: no nonce left, renderer attributes kept, the document's script gone", () => {
    const result = renderDocument(createEngine(),
      body("# H\n\n<script>window.__mdpRan = 1</script>\n\n<p class=\"code-line\" data-line=\"9\">x</p>"),
      DOC, SETTINGS, createMermaid());
    const host = document.createElement("div");
    host.append(result.fragment);
    expect(host.querySelector("[data-eitri-n]")).toBeNull();
    expect(host.querySelector("h1")!.className).toBe("code-line");
    expect(host.querySelector("script")).toBeNull();
    expect(host.querySelector('p[data-line="9"]')).toBeNull();
    expect([result.buf, result.tick, result.name]).toEqual([3, 41, "/p/README.md"]);
  });

  it("fills math, hands diagrams to the runner, counts what it drew and traces it", () => {
    const info = vi.spyOn(console, "info").mockImplementation(() => {});
    const runner: MermaidRunner = { fill: vi.fn(() => 0), renderPending: vi.fn(), setTheme: vi.fn() };
    const FENCE = "`".repeat(3);
    const result = renderDocument(createEngine(), body(
      `# T\n\n$x$ ~~s~~ https://example.org[^1]\n\n| a |\n|---|\n| 1 |\n\n- [ ] t\n\n> [!TIP]\n> t\n\n` +
      `${FENCE}mermaid\ngraph TD\n${FENCE}\n\n![a](http://example.org/a.png) ![b](x.png)\n\n[^1]: n\n`),
      DOC, SETTINGS, runner);
    expect(runner.fill).toHaveBeenCalledWith(result.fragment);
    const host = document.createElement("div");
    host.append(result.fragment);
    expect(host.querySelector('[data-eitri-math="inline"] .katex')).not.toBeNull();
    expect(result.blocked).toBe(1);
    const line = info.mock.calls.map((c) => String(c[0])).find((s) => s.startsWith("[page-trace] rendered "))!;
    const data = JSON.parse(line.slice("[page-trace] rendered ".length));
    expect(data).toMatchObject({ buf: 3, tick: 41, math: 1, mermaid: 1, alerts: 1, frontmatter: 0, images_shown: 1,
      images_blocked: 1 });
    expect(data.kinds).toEqual(expect.arrayContaining(
      ["anchor", "autolink", "footnote", "heading", "math", "mermaid", "alert", "strikethrough", "table", "task"]));
    expect(result.counts.table).toBe(1);
  });

  it("keeps raw HTML that stops inside a tag from taking over the renderer's next tag", () => {
    for (const tail of ['', ' title="', " title='", " title="]) {
      const result = renderDocument(createEngine(),
        body(`intro\n\n<div class="mdp-current-bar" data-line="999"${tail}\n\npara\n`), DOC, SETTINGS, createMermaid());
      const host = document.createElement("div");
      host.append(result.fragment);
      expect(host.querySelector(".mdp-current-bar, [data-line='999']"), tail).toBeNull();
      expect([...host.querySelectorAll("p.code-line")].map((p) => [p.getAttribute("data-line"), p.textContent]), tail)
        .toEqual([["0", "intro"], ["4", "para"]]);
    }
  });

  it("boxes https images when remote images are off", () => {
    const result = renderDocument(createEngine(), body("![a](https://example.org/a.png)"), DOC,
      { ...SETTINGS, remote_images: "off" }, createMermaid());
    const host = document.createElement("div");
    host.append(result.fragment);
    expect(host.querySelector("img")).toBeNull();
    expect(host.querySelector('.mdp-box[data-eitri-blocked="remote_off"]')).not.toBeNull();
  });
});
