import { describe, expect, it } from "vitest";
import { highlightCode, LANGUAGES } from "./highlight";

const SAMPLES: Record<string, string> = {
  bash: 'echo "hi" # c',
  c: "int main(void) { return 0; }",
  cpp: "#include <vector>\nint x = 1;",
  csharp: "public class A { }",
  go: "func main() {}",
  rust: "fn main() { let x = 1; }",
  python: "def f(x):\n    return x  # c",
  javascript: "const x = 1;",
  typescript: "let x: number = 1;",
  java: "public class A {}",
  kotlin: "fun main() {}",
  lua: "local x = 1",
  ruby: "def f; end",
  php: "<?php echo 1; ?>",
  sql: "SELECT 1;",
  yaml: "key: value",
  ini: "[s]\nk = 1",
  json: '{"a": 1}',
  xml: '<a href="x">t</a>',
  css: "a { color: red; }",
  markdown: "# Title",
  diff: "+added\n-removed",
  dockerfile: "FROM debian\nRUN echo hi",
  makefile: "all:\n\techo hi",
  plaintext: "just text",
};

describe("highlightCode", () => {
  it("highlights a sample in each registered grammar", () => {
    for (const [lang, code] of Object.entries(SAMPLES)) {
      const html = highlightCode(code, lang, "n1");
      expect(html, lang).not.toBeNull();
      if (lang !== "plaintext") expect(html, lang).toContain('class="hljs-');
    }
    expect(highlightCode("just text", "plaintext", "n1")).toBe("just text");
  });

  it("lists every grammar and the aliases a fence is written with, sorted, once each", () => {
    const names = [
      ...Object.keys(SAMPLES),
      "sh", "shell", "zsh", "h", "hpp", "cs", "golang", "rs", "py", "js", "ts", "kt", "rb", "yml", "toml", "html",
      "md", "patch", "docker", "mk", "text", "txt",
    ];
    for (const name of names) expect(LANGUAGES, name).toContain(name);
    expect([...LANGUAGES]).toEqual([...LANGUAGES].sort());
    expect(new Set(LANGUAGES).size).toBe(LANGUAGES.length);
  });

  it("finds a language whatever its case and the spaces around it", () => {
    expect(highlightCode("fn x() {}", " Rust ", "n1")).toContain("hljs-keyword");
    expect(highlightCode("echo hi", "shell", "n1")).toContain("hljs-built_in");
    expect(highlightCode("a = 1", "toml", "n1")).toContain("hljs-attr");
  });

  it("returns null for an unknown or empty language", () => {
    for (const lang of ["brainfuck", "", "   ", "mermaid", "math"]) expect(highlightCode("x", lang, "n1"), lang).toBeNull();
  });

  it("puts the nonce on every span it writes, and only there", () => {
    for (const [lang, code] of Object.entries(SAMPLES)) {
      const html = highlightCode(code, lang, "n1") ?? "";
      const spans = html.match(/<span\b[^>]*>/g) ?? [];
      for (const span of spans) expect(span, lang).toMatch(/^<span data-eitri-n="n1" class="(hljs-|language-)/);
      expect((html.match(/data-eitri-n="n1"/g) ?? []).length, lang).toBe(spans.length);
    }
  });

  it("escapes the HTML a code block holds", () => {
    const html = highlightCode("<script>alert(1)</script>", "xml", "n1") ?? "";
    expect(html).not.toContain("<script");
    expect(html).toContain("&lt;");
    expect(highlightCode("a < b && c > d", "plaintext", "n1")).toBe("a &lt; b &amp;&amp; c &gt; d");
  });
});
