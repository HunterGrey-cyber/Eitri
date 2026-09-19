/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import css from "./index.css?raw";
import { hljs } from "./markdown";

/*
 * Spec §7 asks for a test of the highlight.js class -> token mapping. Nothing before this asserted
 * that the class SET the registered languages actually emit is covered by `index.css` -- the
 * mapping in `markdown.ts`'s CSS comment was written by hand against a guess at what each grammar
 * emits, and two real gaps slipped through that guess: `.hljs-function` (Go wraps a whole `func
 * name(params)` signature in one outer span with this class) and `.hljs-section` (the `ini`
 * grammar's `[section]` header, which is what `toml` is registered from) were both live and
 * unstyled until this test found them.
 *
 * This uses the SAME `hljs` instance `markdown.ts` registers and exports for exactly this purpose
 * -- not a second copy of the registration list -- so a language added or removed there is
 * automatically in or out of this coverage check too.
 */

/** One small, realistic sample per registered language, each written to exercise a spread of
 *  syntax groups (a comment, a string, a number, a keyword, a function/type declaration) rather
 *  than the bare one-liners `markdown.test.ts` uses for its narrower assertions. Not claimed to be
 *  exhaustive of every mode every grammar can reach -- see the report for that limit -- but broad
 *  enough that it is what found the two real gaps above. */
const SAMPLES: Record<string, string> = {
  rust: `// A comment
#[derive(Debug)]
struct Point { x: i32, y: i32 }
fn main() {
    let name = "world";
    println!("hello {}", name);
    let n: i32 = 42;
}
`,
  typescript: `// comment
interface Point { x: number; y: number; }
function add(a: number, b: number): number {
  return a + b;
}
const s = "hello";
`,
  javascript: `// comment
function add(a, b) {
  return a + b;
}
const s = "hello";
class Foo extends Bar {}
`,
  python: `# comment
def add(a, b):
    return a + b

class Foo(Bar):
    def __init__(self):
        self.x = 1

s = "hello"
n = 42
`,
  go: `// comment
package main

import "fmt"

func main() {
    s := "hello"
    n := 42
    fmt.Println(s, n)
}
`,
  lua: `-- comment
local function add(a, b)
  return a + b
end
local s = "hello"
local n = 42
`,
  bash: `#!/bin/bash
# comment
NAME="world"
echo "hello $NAME"
if [ -z "$NAME" ]; then
  exit 1
fi
`,
  json: `{
  "name": "hello",
  "count": 42,
  "enabled": true
}
`,
  toml: `# comment
[section]
name = "hello"
count = 42
enabled = true
`,
  yaml: `# comment
name: hello
count: 42
items:
  - one
  - two
`,
  diff: `--- a/file.txt
+++ b/file.txt
@@ -1,2 +1,2 @@
-old line
+new line
 context line
`,
};

/** Every `hljs-*` class that appears in a `class="..."` attribute anywhere in `html`. Ignores
 *  non-`hljs-`-prefixed modifier classes highlight.js pairs onto a base class (`function_`,
 *  `class_`, `inherited__`, `language_`) -- those ride along with a base `hljs-*` class a plain CSS
 *  class selector already matches regardless of what else is on the element, so they need no rule
 *  of their own. */
function hljsClasses(html: string): Set<string> {
  const classes = new Set<string>();
  for (const attr of html.match(/class="[^"]+"/g) ?? []) {
    for (const token of attr.slice(7, -1).split(/\s+/)) {
      if (token.startsWith("hljs-")) classes.add(token);
    }
  }
  return classes;
}

/** Every class name a `.hljs-<name>` (or compound `.hljs-<name>.<modifier>`) selector targets
 *  anywhere in `index.css`. A plain class selector matches an element carrying at least that
 *  class, so a compound selector's leading `.hljs-<name>` is enough to cover the base class. */
function stylesHljsClasses(source: string): Set<string> {
  const styled = new Set<string>();
  for (const match of source.match(/\.hljs-[\w-]+/g) ?? []) {
    styled.add(match.slice(1));
  }
  return styled;
}

describe("highlight.js class -> index.css coverage", () => {
  it("styles every hljs-* class these samples actually emit", () => {
    const emitted = new Set<string>();
    for (const [language, code] of Object.entries(SAMPLES)) {
      const html = hljs.highlight(code, { language, ignoreIllegals: true }).value;
      for (const cls of hljsClasses(html)) emitted.add(cls);
    }
    // A real sample set must produce more than a token gesture, or this test proves nothing.
    expect(emitted.size).toBeGreaterThan(10);

    const styled = stylesHljsClasses(css);
    const uncovered = [...emitted].filter((cls) => !styled.has(cls));
    expect(uncovered).toEqual([]);
  });
});
