import MarkdownIt from "markdown-it";
import { describe, expect, it } from "vitest";
import { alertKind, alertsPlugin, type AlertKind } from "./alerts";

function render(src: string, extra?: (md: MarkdownIt) => void): HTMLElement {
  const md = new MarkdownIt();
  alertsPlugin(md);
  extra?.(md);
  const host = document.createElement("div");
  host.innerHTML = md.render(src, { nonce: "n1" });
  return host;
}

const TABLE: [AlertKind, string[]][] = [
  ["note", ["note", "info", "todo", "abstract", "summary", "tldr", "quote", "cite", "example"]],
  ["tip", ["tip", "hint", "success", "check", "done"]],
  ["important", ["important", "question", "help", "faq"]],
  ["warning", ["warning", "attention"]],
  ["caution", ["caution", "danger", "error", "failure", "fail", "missing", "bug"]],
];

describe("alertKind", () => {
  it("maps every GitHub and Obsidian word onto one of GitHub's five colours, in any case", () => {
    for (const [kind, words] of TABLE) {
      for (const word of words) {
        expect(alertKind(word), word).toBe(kind);
        expect(alertKind(word.toUpperCase()), word).toBe(kind);
      }
    }
  });
  it("maps an unknown word, an inherited property name included, to note", () => {
    for (const word of ["foo", "constructor", "toString", "__proto__", ""]) expect(alertKind(word), word).toBe("note");
  });
});

describe("alertsPlugin", () => {
  it("renders GitHub's five kinds with their default titles, any letter case", () => {
    for (const [marker, kind, title] of [
      ["NOTE", "note", "Note"], ["TIP", "tip", "Tip"], ["IMPORTANT", "important", "Important"],
      ["Warning", "warning", "Warning"], ["caution", "caution", "Caution"],
    ]) {
      const box = render(`> [!${marker}]\n> body text`).firstElementChild!;
      expect(box.tagName, marker).toBe("DIV");
      expect(box.classList.contains("markdown-alert"), marker).toBe(true);
      expect(box.classList.contains(`markdown-alert-${kind}`), marker).toBe(true);
      expect(box.getAttribute("data-eitri-n"), marker).toBe("n1");
      const head = box.querySelector(".markdown-alert-title")!;
      expect(head.tagName).toBe("P");
      expect(head.getAttribute("data-eitri-n")).toBe("n1");
      expect(head.textContent).toBe(title);
      expect(box.querySelector("p:not(.markdown-alert-title)")!.textContent).toBe("body text");
    }
  });

  it("accepts Obsidian's words and colours them by the table", () => {
    for (const [kind, words] of TABLE) {
      for (const word of words) {
        const box = render(`> [!${word}]\n> x`).firstElementChild!;
        expect(box.classList.contains(`markdown-alert-${kind}`), word).toBe(true);
      }
    }
  });

  it("titles an unknown word with the word and colours it as a note", () => {
    const box = render("> [!foo]\n> x").firstElementChild!;
    expect(box.classList.contains("markdown-alert-note")).toBe(true);
    expect(box.querySelector(".markdown-alert-title")!.textContent).toBe("Foo");
  });

  it("takes a custom title from the marker line, as text", () => {
    expect(render("> [!note] Read this first\n> x").querySelector(".markdown-alert-title")!.textContent)
      .toBe("Read this first");
    const box = render("> [!note] <b>bold</b>\n> x");
    expect(box.querySelector(".markdown-alert-title")!.textContent).toBe("<b>bold</b>");
    expect(box.querySelector("b")).toBeNull();
  });

  it("draws the fold markers as details, closed for - and open for +", () => {
    const closed = render("> [!note]-\n> hidden").firstElementChild!;
    expect(closed.tagName).toBe("DETAILS");
    expect(closed.hasAttribute("open")).toBe(false);
    expect(closed.classList.contains("markdown-alert-note")).toBe(true);
    const summary = closed.querySelector("summary")!;
    expect(summary.classList.contains("markdown-alert-title")).toBe(true);
    expect(summary.getAttribute("data-eitri-n")).toBe("n1");
    expect(summary.textContent).toBe("Note");
    const open = render("> [!tip]+ Open one\n> x").firstElementChild!;
    expect(open.tagName).toBe("DETAILS");
    expect(open.hasAttribute("open")).toBe(true);
    expect(open.querySelector("summary")!.textContent).toBe("Open one");
    expect(render("> [!warning]- Folded title\n> x").querySelector("summary")!.textContent).toBe("Folded title");
  });

  it("leaves a plain quote and a broken marker a blockquote", () => {
    expect(render("> plain quote").firstElementChild!.tagName).toBe("BLOCKQUOTE");
    expect(render("> [!note\n> x").firstElementChild!.tagName).toBe("BLOCKQUOTE");
  });

  it("closes an alert at its own end when a quote is nested in it; an alert inside an alert is a quote, as on GitHub", () => {
    const box = render("> [!NOTE]\n> > inner\n>\n> after").firstElementChild!;
    expect(box.classList.contains("markdown-alert-note")).toBe(true);
    expect(box.querySelector(":scope > blockquote")!.textContent!.trim()).toBe("inner");
    expect([...box.querySelectorAll(":scope > p")].map((p) => p.textContent)).toEqual(["Note", "after"]);
    const outer = render("> [!NOTE]\n> > [!TIP]\n> > tip text\n>\n> note text").firstElementChild!;
    expect(outer.classList.contains("markdown-alert-note")).toBe(true);
    expect(outer.querySelector(".markdown-alert-tip")).toBeNull();
    expect(outer.querySelector(":scope > blockquote")!.textContent).toContain("tip text");
    expect(outer.querySelector(":scope > p:not(.markdown-alert-title)")!.textContent).toBe("note text");
  });

  it("keeps the token's source lines and writes every attribute a later core rule puts on it", () => {
    const md = new MarkdownIt();
    alertsPlugin(md);
    const opened = md.parse("intro\n\n> [!NOTE]\n> a\n> b\n", { nonce: "n1" }).find((t) => t.type === "alert_open")!;
    expect(opened.map).toEqual([2, 5]);
    const box = render("> [!NOTE]\n> a", (m) => m.core.ruler.push("tag", (state) => {
      for (const t of state.tokens) {
        if (t.type === "alert_open") { t.attrSet("data-line", String(t.map![0])); t.attrJoin("class", "code-line"); }
      }
    })).firstElementChild!;
    expect(box.getAttribute("data-line")).toBe("0");
    expect([...box.classList].sort()).toEqual(["code-line", "markdown-alert", "markdown-alert-note"]);
  });
  it("searches alert bodies in time proportional to the number of alerts", () => {
    const md = new MarkdownIt();
    alertsPlugin(md);
    const time = (n: number) => {
      const source = "> [!NOTE]\n> body\n\n".repeat(n);
      let best = Number.POSITIVE_INFINITY;
      for (let i = 0; i < 3; i++) {
        const start = performance.now();
        md.parse(source, { nonce: "n1" });
        best = Math.min(best, performance.now() - start);
      }
      return Math.max(best, 0.05);
    };
    time(500);
    expect(time(16000) / time(1000)).toBeLessThan(64);
  });

});
