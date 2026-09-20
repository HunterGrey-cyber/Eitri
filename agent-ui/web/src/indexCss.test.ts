// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, describe, expect, it } from "vitest";
import css from "./index.css?raw";
// The Rust side of the theme contract, read as TEXT at test time. See
// "every --nv-* this stylesheet reads is one Rust actually emits" at the bottom of this file for
// why the allowed names are derived from this source rather than written down here.
import tokensRs from "../../../core/src/theme/tokens.rs?raw";

/** Strip CSS comments, but never a `/*` that is inside a string.
 *
 *  The eighth bypass, found by the round-2 re-review, and it sat UPSTREAM of every scan in this
 *  file rather than inside any one guard: with a plain regex, a `content` string holding an
 *  opening comment marker and a later one holding a closing marker delete everything between
 *  them -- real rules included -- before a guard ever sees the text, and the braces stay balanced
 *  so `splitRules`' own throw does not fire. `splitRules` was already rewritten to treat a brace
 *  inside a string as literal text; this is that same awareness one step earlier, where the input
 *  to all of it is produced. */
function stripComments(sheet: string): string {
  let out = "";
  let quote: string | null = null;
  for (let i = 0; i < sheet.length; i += 1) {
    const c = sheet[i];
    if (quote !== null) {
      out += c;
      if (c === "\\") {
        i += 1;
        if (i < sheet.length) out += sheet[i];
      } else if (c === quote) {
        quote = null;
      }
      continue;
    }
    if (c === '"' || c === "'") {
      quote = c;
      out += c;
      continue;
    }
    if (c === "/" && sheet[i + 1] === "*") {
      const end = sheet.indexOf("*/", i + 2);
      i = end === -1 ? sheet.length : end + 1;
      continue;
    }
    out += c;
  }
  return out;
}

const withoutComments = stripComments(css);

/** Whether EVERY selector in a comma-separated list matches `branch` -- not just one of them. A
 *  grouped selector like `.session-lost, .hljs-keyword` must not smuggle a colour past a surface's
 *  exemption just because one branch of the group happens to sit on that surface; every branch of
 *  the group is painted by the rule, so every branch must earn it. Shared by the `hljs-*` and
 *  `winbar`/`status-line` surfaces below rather than each carrying its own copy. */
function everyBranchIsOnSurface(selector: string, branch: RegExp): boolean {
  const selectors = selector.split(",").map((s) => s.trim());
  return selectors.length > 0 && selectors.every((s) => branch.test(s));
}

const HLJS_BRANCH = /(?:^|[\s.])hljs-[\w-]+/;
/** A selector branch that sits inside the winbar or the status line -- `.winbar`, `.status-line`,
 *  or either as an ancestor (`.winbar .identities`, `.status-line .position`, ...). */
const CHROME_BRANCH = /(?:^|[\s.])(?:winbar|status-line)\b/;
/** The solid cursor block: the current row's sign cell, and a focused button with its children
 *  (every control is keyboard-reachable and the selected one is drawn as the cursor). Nothing else. */
const CURSOR_BRANCH = /^(?:\.row-current \.row-sign|\.agent-ui-root button:focus(?: \*)?)$/;
/** The global `f` HINT's label, and nothing else (spec 2026-09-19-global-hint-design.md §2.3). */
const HINT_BRANCH = /^\.hint-label$/;

/**
 * The rule this file actually enforces, in one sentence: **a colour may be used as text only where
 * Rust guards it for text against the surface it sits on.** `tokens.rs` guards `fg`/`muted` for
 * text against `bg` (`TEXT_CONTRAST`, 4.5:1) -- the panel body's surface -- and separately guards
 * `chrome-fg`/`chrome-muted` for text against `chrome` (also 4.5:1, `tokens.rs:209-210`) -- the
 * winbar/status-line's surface, painted in nvim's own `StatusLine` colours rather than the body's.
 * Neither pair is interchangeable with the other: `chrome`-guarded text on the plain body, or
 * `bg`-guarded text on chrome, was never measured against the surface actually under it. The one
 * other exemption (panel-as-document task 4) is `--nv-syn-*` inside a `hljs-*` rule -- deliberately
 * UNguarded, because syntax tokens are meant to be the editor's own colours; see the big comment on
 * the real guard test below for why that one is not a contrast exemption at all.
 *
 * Returns every `color:` declaration in `source` that is NOT one of the three sanctioned pairings
 * above. Shared by the real guard test and the tests proving none of the three exemptions can
 * quietly widen -- on purpose. Before this was hoisted, each test carried its own copy of this
 * logic over its own string literal, so a change to the real guard (say, widening a regex to admit
 * more than intended) had no effect on the proof tests' copies at all. Calling the same function
 * from all of them closes that: a widened regex here is exercised by, and must still satisfy,
 * every proof test too.
 */
function unguardedTextColorDeclarations(source: string): string[] {
  const rules = source.match(/[^{}]*\{[^{}]*\}/g) ?? [];
  const declarations: string[] = [];
  for (const rule of rules) {
    const selector = rule.slice(0, rule.indexOf("{"));
    const onHljsSurface = everyBranchIsOnSurface(selector, HLJS_BRANCH);
    const onChromeSurface = everyBranchIsOnSurface(selector, CHROME_BRANCH);
    const body = rule.slice(rule.indexOf("{") + 1, -1);
    const onCursorSurface =
      everyBranchIsOnSurface(selector, CURSOR_BRANCH) && /(?:^|;)\s*background: var\(--nv-fg\);/.test(body);
    const onHintSurface =
      everyBranchIsOnSurface(selector, HINT_BRANCH) && /(?:^|;)\s*background: var\(--nv-hint-bg\);/.test(body);
    for (const declaration of body.match(/(?<![a-z-])color:[^;]*;/g) ?? []) {
      if (onHljsSurface && /^color: var\(--nv-syn-[a-z]+\);$/.test(declaration)) continue;
      if (onChromeSurface && /^color: var\(--nv-chrome-(fg|muted)\);$/.test(declaration)) continue;
      // The panel's cursor: `--nv-bg` text on an `--nv-fg` fill, the body's own pair reversed.
      // Only where the SAME rule paints that fill, so the exemption cannot outlive the fill.
      if (onCursorSurface && /^color: var\(--nv-bg\);$/.test(declaration)) continue;
      // A HINT label: `--nv-hint-fg` on the `--nv-hint-bg` fill it is guarded against in Rust
      // (`tokens.rs`, 4.5:1). Again only where the same rule paints that fill.
      if (onHintSurface && /^color: var\(--nv-hint-fg\);$/.test(declaration)) continue;
      declarations.push(declaration);
    }
  }
  return declarations;
}

describe("index.css", () => {
  it("contains no colour literal -- every colour comes from nvim through --nv-* variables", () => {
    expect(withoutComments.match(/#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(/g)).toBeNull();
  });

  it("names no font family -- both stacks come from Rust", () => {
    const families = withoutComments.match(/font-family:[^;]*;/g) ?? [];
    expect(families.length).toBeGreaterThan(0);
    for (const declaration of families) {
      expect(declaration).toMatch(/^font-family: var\(--nv-font-(prose|mono)\);$/);
    }
  });

  it("uses --nv-border only for rules, never as a fill under text", () => {
    // --nv-border is WinSeparator's foreground: a line colour nothing guards against text. Under
    // nvim's built-in default scheme it equals --nv-fg, so a card filled with it hides its heading.
    const declarations = withoutComments.match(/[a-z-]+:[^;{}]*var\(--nv-border\)[^;{}]*;/g) ?? [];
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^border(-[a-z]+)?:/);
    }
  });

  it("draws text only where Rust guards that colour for text against the surface it sits on", () => {
    // --nv-warn/--nv-error/--nv-ok are guarded at 3:1 -- for borders, rules, tints and dots (WCAG
    // 1.4.11), not for text -- and --nv-mode-browse/--nv-mode-input/--nv-chrome-accent are guarded
    // at 3:1 or not at all. Used as 12px text on rose-pine dawn --nv-fg/--nv-muted's old ungated
    // uses measured 2.05:1 (a resume row's heading on its hover band) to 3.84:1 (the session-lost
    // banner), where the same text had been about 12:1 before the theme pipeline. A signal colour
    // goes on a border or a rule beside the text, never on the text.
    // (`(?<![a-z-])` so `background-color:`/`border-color:` are not read as text colours.)
    //
    // Two sanctioned pairings, each a colour guarded for TEXT against the specific surface it is
    // drawn on: `--nv-fg`/`--nv-muted` on the plain body (guarded against `bg`), and
    // `--nv-chrome-fg`/`--nv-chrome-muted` (panel-as-document task 6, fix round 1) inside the
    // winbar/status-line (guarded against `chrome` instead -- a reversed StatusLine makes `chrome`
    // Normal's `fg`, so checking against `bg` would measure the wrong pair). Plus one exemption
    // that is NOT a contrast guarantee at all (panel-as-document task 4): `--nv-syn-*` inside a
    // `hljs-*` rule, deliberately unguarded because syntax tokens are meant to be the editor's own
    // colours -- contrast-protecting them would make the panel's code disagree with the buffer
    // beside it, which is precisely the feature, and a colorscheme whose `@comment` is unreadable
    // renders unreadable comments in nvim too. Each exemption is narrowed to its own selector
    // scope -- ENTIRELY `hljs-*`, or ENTIRELY inside `.winbar`/`.status-line` -- so none of the
    // three can quietly widen to cover an unrelated rule or a grouped selector with only one
    // branch on the right surface; the proof tests below pin all three directions.
    const declarations = unguardedTextColorDeclarations(withoutComments);
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^color: var\(--nv-(fg|muted)\);$/);
    }
  });

  it("still refuses a signal colour as text, even inside an hljs-* rule", () => {
    // Proves the narrowed guard above did not quietly become "anything inside .hljs-* is exempt" --
    // only --nv-syn-* survives it. A signal colour on text is exactly what the guard exists to
    // catch, and this calls the SAME `unguardedTextColorDeclarations` the real guard test above
    // does, so a future widening of that function's exemption regex fails HERE, not just silently
    // passes both tests the way two independent copies of the same logic once did.
    const declarations = unguardedTextColorDeclarations(".hljs-keyword { color: var(--nv-warn); }");
    expect(declarations).toEqual(["color: var(--nv-warn);"]);
    expect(declarations[0]).not.toMatch(/^color: var\(--nv-(fg|muted)\);$/);
  });

  it("requires every branch of a grouped selector to be hljs-*, not just one", () => {
    // `.session-lost, .hljs-keyword { color: var(--nv-syn-keyword); }` paints BOTH selectors with
    // a syntax-token colour, but only one of them is a syntax-highlighting rule -- `.session-lost`
    // would render actual UI text in an editor-derived colour nothing guards for contrast. The
    // exemption must require every comma-separated branch to be `hljs-*`, not merely one.
    const declarations = unguardedTextColorDeclarations(
      ".session-lost, .hljs-keyword { color: var(--nv-syn-keyword); }",
    );
    expect(declarations).toEqual(["color: var(--nv-syn-keyword);"]);
    expect(declarations[0]).not.toMatch(/^color: var\(--nv-(fg|muted)\);$/);
  });

  it("still refuses --nv-chrome-accent as text inside the winbar", () => {
    // `chrome_accent` is guarded at UI_CONTRAST (3.0), the WCAG 1.4.11 non-text threshold -- not
    // the 4.5:1 a 12px label needs, and `tokens.rs`'s own test asserts only `>= 3.0`. Only
    // chrome-fg/chrome-muted are guarded for TEXT against chrome, so only those two survive the
    // chrome exemption; chrome-accent must still fail here even though it is scoped to `.winbar`,
    // the same way a signal colour still fails inside `.hljs-*` above.
    //
    // This test's name used to end "despite its own doc comment", because that doc read "Accent
    // TEXT drawn on chrome (the status bar's mode label)" -- a description that invited exactly the
    // use this refuses. The doc now names its tier instead, so the two agree and there is no
    // "despite" left; the guard stays, because the doc agreeing today is not a mechanism.
    const declarations = unguardedTextColorDeclarations(".winbar .mode-label { color: var(--nv-chrome-accent); }");
    expect(declarations).toEqual(["color: var(--nv-chrome-accent);"]);
  });

  it("requires every branch of a grouped selector to be on the chrome surface, not just one", () => {
    // The same smuggling `.session-lost, .hljs-keyword` proves above, for the winbar/status-line
    // exemption: `.session-lost` is not part of either bar and must not borrow their guarantee just
    // because it shares a rule with something that is.
    const declarations = unguardedTextColorDeclarations(
      ".session-lost, .status-line .position { color: var(--nv-chrome-muted); }",
    );
    expect(declarations).toEqual(["color: var(--nv-chrome-muted);"]);
    expect(declarations[0]).not.toMatch(/^color: var\(--nv-(fg|muted)\);$/);
  });

  it("admits --nv-bg as text on a focused button only where the rule paints the --nv-fg fill", () => {
    const rule = ".agent-ui-root button:focus, .agent-ui-root button:focus * { background: var(--nv-fg); color: var(--nv-bg); }";
    expect(unguardedTextColorDeclarations(rule)).toEqual([]);
    expect(unguardedTextColorDeclarations(".agent-ui-root button:focus { color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
    expect(unguardedTextColorDeclarations(".agent-ui-root button { background: var(--nv-fg); color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
  });

  it("admits --nv-bg as text only on the cursor cell that is filled with --nv-fg", () => {
    const filled = ".row-current .row-sign { background: var(--nv-fg); color: var(--nv-bg); }";
    expect(unguardedTextColorDeclarations(filled)).toEqual([]);
    // The same colour with the fill gone is --nv-bg on --nv-bg: invisible, and refused.
    expect(unguardedTextColorDeclarations(".row-current .row-sign { color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
    // Anywhere else it is refused, and so is a group with one branch on the cursor cell.
    expect(unguardedTextColorDeclarations(".row-sign { background: var(--nv-fg); color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
    expect(
      unguardedTextColorDeclarations(".row-current .row-sign, .row-body { background: var(--nv-fg); color: var(--nv-bg); }"),
    ).toEqual(["color: var(--nv-bg);"]);
  });

  it("admits --nv-hint-fg as text only on a HINT label filled with --nv-hint-bg", () => {
    // `tokens.rs` guards `hint_fg` for text against `hint_bg` (4.5:1) and against nothing else, so
    // the pair is only sound where the SAME rule lays that fill under the text.
    const filled = ".hint-label { background: var(--nv-hint-bg); color: var(--nv-hint-fg); }";
    expect(unguardedTextColorDeclarations(filled)).toEqual([]);
    // Negative controls: the colour without its fill, on any other selector, or in a group with
    // only one branch on the label.
    expect(unguardedTextColorDeclarations(".hint-label { color: var(--nv-hint-fg); }")).toEqual([
      "color: var(--nv-hint-fg);",
    ]);
    expect(unguardedTextColorDeclarations(".row-sign { background: var(--nv-hint-bg); color: var(--nv-hint-fg); }")).toEqual([
      "color: var(--nv-hint-fg);",
    ]);
    expect(
      unguardedTextColorDeclarations(".hint-label, .row-body { background: var(--nv-hint-bg); color: var(--nv-hint-fg); }"),
    ).toEqual(["color: var(--nv-hint-fg);"]);
  });

  it("paints the HINT label with the IncSearch pair, and gives its layer a containing block", () => {
    // `splitRules` (defined with the motion guards below) rather than a regex splitter: it is the
    // one in this file that cannot mistake a nested block's `{` for its parent's.
    const rules = splitRules(withoutComments);
    const label = rules.find((r) => r.selector === ".hint-label");
    expect(label).toBeDefined();
    expect(label?.declarations).toMatch(/background: var\(--nv-hint-bg\);/);
    expect(label?.declarations).toMatch(/(?<![a-z-])color: var\(--nv-hint-fg\);/);
    // The layer is `position: absolute; inset: 0` over the panel root, and the labels are placed
    // from rects measured against that root, so the root must be their containing block.
    const root = rules.find((r) => r.selector === ".agent-ui-root");
    expect(root?.declarations).toMatch(/position: relative;/);
  });

  it("does not dim a conversation-picker row with opacity", () => {
    // Every remembered-session row is unselected until clicked, since the picker opens on "New
    // session". Opacity multiplies text contrast down with it: fg at 0.7 is 3.34:1 on rose-pine
    // dawn and 2.93:1 on its hover band. The selected row is marked by its fill instead.
    //
    // `row-choice` as well as `session-choice`: the picker's rows moved onto the shared `.row`
    // grid, and the rule that could dim one is now `.row-choice`'s, which the old pattern did not
    // match at all.
    const rules = withoutComments.match(/[^{}]*(session-choice|row-choice)[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("does not dim the winbar's identity text with opacity either", () => {
    // Same defect, same fix, same reason: opacity on top of an already-guarded chrome-muted text
    // colour would multiply its contrast back down below what tokens.rs actually guaranteed.
    const rules = withoutComments.match(/\.winbar[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  /* In-flight motion (2026-09-20-in-flight-motion-design.md) §5.3's two structural assertions: "the
     element is absent" <=> "no animation is running" has to be a property of the FILE, not of this
     document, so a second animated selector or a second @keyframes reusing the idea would be a
     silent second motion nobody argued for (design §11: "no second animation for a second
     purpose").

     **Rewritten by the whole-branch review of 2026-09-20, which demonstrated three passing bypasses
     against the real file, and again by its re-review the same day, which demonstrated three more
     against the rewrite.** All six are kept below as negative controls, fed to the same splitter
     the real assertions use -- as STRINGS rather than as real CSS, so a future author who reopens
     one of them fails a test here instead of shipping an unargued motion:

       (a) `animation-name` / `animation-duration` longhands on a second selector. The detector was
           `/(?<![a-z-])animation:/`, the SHORTHAND only, so the longhand spelling of the same thing
           was invisible.
       (b) `transition: background-color 300ms` on any selector. The word `transition` appeared
           nowhere in this file, and animating a COLOUR is exactly what §7.1 forbids -- and the
           single most ordinary thing to reach for on a hover state.
       (c) a keyframe's LAST declaration written with no trailing `;` (`to { transform: scaleX(1);
           opacity: 1 }`). The declaration regex was `[a-z-]+:[^;]*;`, which needs a later `;` to
           terminate the match, so the one declaration that most often lacks one was never checked.
       (d) a NESTED block hiding its parent's motion: `.tool-card { transition: ...; &:hover { ... } }`.
           The rule splitter was `/[^{}]*\{[^{}]*\}/g`, whose `[^{}]*` cannot span a nested block, so
           the only match it produced began inside the parent -- and the old `declarationsOf` then
           sliced after that match's first `{`, which is the CHILD's, classifying the parent's
           `transition:` as selector text. Closing (b)'s false positive on a class NAMED `.transition`
           is what opened this, and it was a REGRESSION: the guard before (b), which searched whole
           rule text, did catch this one.
       (e) the same nesting one level down, inside `@media`.
       (f) `content: "}"` followed by `animation:` in one rule -- a brace inside a string, which
           splits the rule in the wrong place for any scanner that does not know about quotes.

     The repair is the splitter itself: `splitRules` walks the sheet tracking brace depth AND quote
     state, so (d), (e) and (f) are all parsed the way a browser parses them. Its companion is the
     "contains no nested blocks today" assertion below, which is what keeps the several NARROWER
     regex scans elsewhere in this file (`.winbar[^{}]*\{[^}]*\}` and friends) sound too: they all
     assume a rule body has no `{` in it, and that assumption now fails loudly rather than silently
     the day someone nests something. */

  /** One CSS block: its prelude, its OWN declarations (a nested block's text belongs to that block,
   *  not to this one), and the selector of the block it sits inside (`null` at the top level). */
  type CssRule = { selector: string; declarations: string; parent: string | null };

  /** Split a stylesheet into blocks, brace-depth and quote aware.
   *
   *  Two things this does that a regex cannot. A nested block is emitted as its OWN rule and its
   *  text is removed from its parent's declarations, so neither can hide the other -- the parent's
   *  own declarations end at the last `;` before the child's prelude, which is where CSS itself
   *  requires them to end. And a `{`, `}` or `;` inside a quoted string is literal text, not
   *  structure. **Throws on unbalanced braces**, so a malformed file fails every test that reads it
   *  rather than quietly yielding truncated blocks. */
  function splitRules(sheet: string): CssRule[] {
    const rules: CssRule[] = [];
    const open: { selector: string; parent: string | null; own: string }[] = [];
    let buffer = "";
    let quote: string | null = null;
    for (let i = 0; i < sheet.length; i++) {
      const ch = sheet[i];
      if (quote !== null) {
        buffer += ch;
        if (ch === "\\") buffer += sheet[++i] ?? "";
        else if (ch === quote) quote = null;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        buffer += ch;
        continue;
      }
      if (ch === "{") {
        // A nested rule's prelude can only begin after its parent's previous declaration was
        // terminated, so the last `;` is the boundary between the two.
        const cut = buffer.lastIndexOf(";");
        const parent = open[open.length - 1];
        if (parent !== undefined && cut !== -1) parent.own += buffer.slice(0, cut + 1);
        open.push({
          selector: (cut === -1 ? buffer : buffer.slice(cut + 1)).trim(),
          parent: parent === undefined ? null : parent.selector,
          own: "",
        });
        buffer = "";
        continue;
      }
      if (ch === "}") {
        const block = open.pop();
        if (block === undefined) throw new Error("unbalanced CSS: a `}` closed no block");
        rules.push({ selector: block.selector, declarations: block.own + buffer, parent: block.parent });
        buffer = "";
        continue;
      }
      buffer += ch;
    }
    if (open.length > 0) throw new Error(`unbalanced CSS: ${open.length} block(s) never closed`);
    return rules;
  }

  /** Anything that MOVES: the `animation` and `transition` shorthands, every longhand of either,
   *  and a vendor-prefixed spelling of any of them. The leading lookbehind is what keeps
   *  `foo-animation:` (not a property) out while letting `-webkit-animation:` in through the
   *  prefix alternative. Matched against a rule's DECLARATIONS only: run over whole rule text it
   *  could be satisfied by a selector (a class literally named `.transition`, or a pseudo-class on
   *  one), which would make the guard pass for the wrong reason.
   *
   *  The `i` flag and the widened lookbehind close the seventh bypass, found by the round-2
   *  re-review: CSS property names are ASCII case-insensitive (CSS Syntax L3), so `TRANSITION:`
   *  is live CSS that a case-sensitive matcher reads as nothing at all. Nobody writes it by
   *  hand -- it is closed because this guard's whole claim is a property of the FILE, and a
   *  property with a spelling that escapes it is not established. An ESCAPED ident is still
   *  NOT matched: decoding those needs a real tokenizer, and that limit is recorded in the
   *  control below rather than left for a ninth review to rediscover. */
  const MOTION_PROPERTY = /(?<![a-zA-Z-])(?:-webkit-|-moz-|-ms-|-o-)?(?:animation|transition)(?:-[a-z-]+)?\s*:/i;
  function movesSomething(rule: CssRule): boolean {
    return MOTION_PROPERTY.test(rule.declarations);
  }

  /** The selectors in a sheet that move something -- the whole guard in one call, so the negative
   *  controls exercise the real splitter rather than a hand-sliced approximation of it. */
  function movingSelectorsIn(sheet: string): string[] {
    // Strips comments HERE rather than taking already-stripped text, so the controls below exercise
    // the whole path a real scan takes -- including the stripper. The eighth bypass lived in that
    // step, and a control that called the stripper itself would have passed with the broken one.
    return splitRules(stripComments(sheet))
      .filter(movesSomething)
      .map((rule) => rule.selector);
  }

  /** Every declaration inside a `@keyframes` body, INCLUDING one written last with no trailing
   *  `;`. Split rather than matched, so a declaration is terminated by `;` or by the end of its
   *  own block -- which is what closes bypass (c). */
  function keyframeDeclarations(body: string): string[] {
    const out: string[] = [];
    for (const block of body.match(/\{[^{}]*\}/g) ?? []) {
      for (const declaration of block.slice(1, -1).split(";")) {
        const trimmed = declaration.trim();
        if (trimmed !== "") out.push(trimmed);
      }
    }
    return out;
  }

  it("contains no nested blocks today, which is what keeps every rule scan in this file sound", () => {
    // Every guard here -- this splitter's callers and the narrower `X[^{}]*\{[^}]*\}` scans alike --
    // assumes a rule body contains no `{`. CSS nesting is supported by the WebKitGTK this ships on
    // and is the ordinary way to write a hover state, so the day someone nests something that
    // assumption has to fail here rather than silently somewhere else. An at-rule (`@media`,
    // `@keyframes`) is a container, not a nested rule, so its children are the allowed case.
    const nested = splitRules(withoutComments).filter((r) => r.parent !== null && !r.parent.startsWith("@"));
    expect(nested.map((r) => `${r.parent} > ${r.selector}`)).toEqual([]);

    // ...and the splitter really can see one, so the assertion above is not vacuous.
    const probe = splitRules(".tool-card { color: red; &:hover { color: blue; } }");
    expect(probe.filter((r) => r.parent !== null && !r.parent.startsWith("@")).map((r) => r.selector)).toEqual([
      "&:hover",
    ]);
    // A block inside `@media` is NOT reported, or the assertion above could never hold.
    const inMedia = splitRules("@media (min-width: 1px) { .a { color: red; } }");
    expect(inMedia.filter((r) => r.parent !== null && !r.parent.startsWith("@"))).toEqual([]);

    // Unbalanced braces throw rather than yielding truncated blocks -- the failure the depth-aware
    // walk would otherwise hide, since a regex splitter simply matches less.
    expect(() => splitRules(".a { color: red; ")).toThrow(/never closed/);
    expect(() => splitRules(".a { color: red; } }")).toThrow(/closed no block/);
  });

  it("puts every animation OR transition declaration on .turn-activity .meter-fill, with exactly one @keyframes", () => {
    const moving = movingSelectorsIn(css);
    expect(moving.length).toBeGreaterThan(0);
    for (const selector of moving) {
      expect(selector).toBe(".turn-activity .meter-fill");
    }
    const keyframeBlocks = withoutComments.match(/@keyframes\s+[\w-]+\s*\{/g) ?? [];
    expect(keyframeBlocks).toHaveLength(1);
    expect(keyframeBlocks[0]).toContain("turn-meter");

    // Negative control 1 (as first written): a second animated selector and a second @keyframes.
    const twoAnimations =
      ".turn-activity .meter-fill { animation: turn-meter 1s; } .row-current { animation: pulse 1s; } " +
      "@keyframes turn-meter { from { transform: scaleX(0.25); } to { transform: scaleX(1); } } " +
      "@keyframes pulse { from { opacity: 0; } to { opacity: 1; } }";
    expect(movingSelectorsIn(twoAnimations)).toContain(".row-current");
    expect((twoAnimations.match(/@keyframes\s+[\w-]+\s*\{/g) ?? []).length).toBeGreaterThan(1);

    // Negative control (a), a REPRODUCED bypass: the longhands. A second element reusing the one
    // permitted @keyframes adds no second @keyframes and writes no `animation:` shorthand, so the
    // original guard passed on it 36/36.
    const longhands = ".row-current .row-sign { animation-name: turn-meter; animation-duration: 2s; }";
    expect(movingSelectorsIn(longhands)).toEqual([".row-current .row-sign"]);
    expect(/(?<![a-z-])animation:/.test(longhands)).toBe(false);

    // Negative control (b), a REPRODUCED bypass: a colour transition. `transition` was not
    // mentioned by any guard in this file, and a 300ms colour fade spends most of its period below
    // the contrast floor `tokens.rs` guarantees only at full strength (§7.1).
    expect(movingSelectorsIn(".tool-card { transition: background-color 300ms linear; }")).toEqual([".tool-card"]);
    expect(movingSelectorsIn(".tool-card:hover { -webkit-transition: color 1s; }")).toEqual([".tool-card:hover"]);
    // ...and it is a DECLARATION guard, not a text search: a class whose NAME contains the word is
    // not a motion, and must not be reported as one.
    expect(movingSelectorsIn(".transition-demo:hover { color: var(--nv-fg); }")).toEqual([]);

    // Negative control (d), a REPRODUCED bypass and a REGRESSION against the guard before (b): a
    // nested block whose own `{` came before the parent's declarations were read. The parent is
    // what moves; the child does not, and neither is lost.
    expect(movingSelectorsIn(".tool-card { transition: background-color 300ms linear; &:hover { color: var(--nv-fg); } }")).toEqual([
      ".tool-card",
    ]);
    // Negative control (e), the same shape one level down, inside an @media.
    expect(
      movingSelectorsIn("@media (min-width: 1px) { .tool-card { transition: opacity 300ms; &:hover { color: var(--nv-fg); } } }"),
    ).toEqual([".tool-card"]);
    // Negative control (f), a REPRODUCED bypass: a brace inside a string. `content: "}"` ends no
    // block, so the `animation:` after it is still this rule's declaration.
    expect(movingSelectorsIn('.row-sign::before { content: "}"; animation: turn-meter 2s; }')).toEqual([
      ".row-sign::before",
    ]);
    // Negative control (g), a REPRODUCED bypass from the round-2 re-review: property names are
    // ASCII case-insensitive per CSS Syntax L3, so this is live CSS that the case-sensitive
    // matcher read as nothing at all. Injected into the real stylesheet it left the suite green.
    expect(movingSelectorsIn(".tool-card { TRANSITION: background-color 300ms linear; }")).toEqual([
      ".tool-card",
    ]);
    // And the limit, stated rather than left for a ninth review to find: an ESCAPED ident is live
    // CSS too (`animati\\6fn` decodes to `animation`) and this guard does NOT catch it, because
    // decoding idents needs a real tokenizer. Asserted as it behaves, so the day someone adds one
    // this line is what tells them the guard was never claiming to.
    expect(movingSelectorsIn(".row-current { animati\\6fn: turn-meter 2s; }")).toEqual([]);
  });

  /// The eighth bypass, and the only one that was upstream of every guard rather than inside one.
  /// A comment stripper that does not know about strings lets two `content` values act as a comment
  /// pair and delete every rule between them -- with the braces still balanced, so `splitRules`'
  /// own throw stays silent. Injected into the real stylesheet, the animated rule in the middle
  /// vanished before any guard ran and the suite stayed green.
  it("does not let a string act as a comment marker", () => {
    const sheet =
      '.a::before { content: "/*"; }\n' +
      ".moving { animation: turn-meter 2s linear infinite; }\n" +
      '.a::after { content: "*/"; }';
    // The rule in the middle survives stripping, so the guard can see it. Fed RAW, through the same
    // entry point every real scan uses -- feeding it pre-stripped would test the stripper and not
    // the path, and would pass with the broken stripper still installed.
    expect(movingSelectorsIn(sheet)).toEqual([".moving"]);
    // ...and a REAL comment is still removed, or this would be a stripper that does nothing.
    expect(stripComments(".x { /* animation: nope; */ color: red; }")).not.toMatch(/animation/);
  });

  it("rests the meter at ONE cell of four under reduced motion, and steps to exactly four cells otherwise", () => {
    // Two arithmetic defects the whole-branch review settled (2026-09-20), kept together because
    // they are the same number seen from two sides.
    //
    // (1) The easing. `steps(4, start)` is `jump-start`, whose progress outputs are
    // {0.25, 0.5, 0.75, 1}; against a `from` of 0.25 that is a scale of 0.25 + 0.75p =
    // {0.4375, 0.625, 0.8125, 1}, i.e. 1.75ch, 2.5ch, 3.25ch, 4ch of a 4ch meter -- never a cell
    // boundary, never fewer than ~1.75 cells, and never the `from` value at all, which made that
    // value's stated purpose ("never empty, so it cannot read as a blink") unreachable.
    // `jump-none` emits {0, 1/3, 2/3, 1}, so the scale is exactly {0.25, 0.5, 0.75, 1}.
    const fill = withoutComments.match(/\.turn-activity \.meter-fill \{[^}]*\}/);
    expect(fill).not.toBeNull();
    expect(fill![0]).toMatch(/animation: turn-meter 1200ms steps\(4, jump-none\) infinite;/);

    // (2) Reduced motion. `animation: none` alone left the computed transform at the initial
    // `none` -- the identity -- so `width: 100%` of the 4ch meter painted a FULL bar, which is the
    // one reading design §4 forbids (a full bar reads as a completed percentage). The static
    // transform is what makes §6's "one cell of four" true, and it must be the keyframes' own
    // `from` value rather than an independently written number, which is why this test DERIVES it
    // instead of spelling it twice.
    const from = withoutComments.match(/@keyframes turn-meter \{\s*from \{ transform: (scaleX\([\d.]+\)); \}/);
    expect(from).not.toBeNull();
    const reduced = withoutComments.match(/@media \(prefers-reduced-motion: reduce\) \{[\s\S]*?\n\}/);
    expect(reduced).not.toBeNull();
    expect(reduced![0]).toContain("animation: none;");
    expect(reduced![0]).toContain(`transform: ${from![1]};`);
    // One quarter, said as arithmetic rather than as a string: one cell of the meter's four.
    expect(Number(from![1].replace(/[^\d.]/g, ""))).toBeCloseTo(1 / 4, 10);

    // Negative control: `animation: none` on its own -- the state this replaced -- carries no
    // transform, so the same extraction finds nothing to rest at.
    const bare = "@media (prefers-reduced-motion: reduce) {\n  .turn-activity .meter-fill {\n    animation: none;\n  }\n}";
    expect(bare).not.toContain("transform:");
  });

  it("keeps the meter's keyframes to transform only -- geometry cannot break the contrast guarantee, colour and alpha can (design §7.1)", () => {
    const match = withoutComments.match(/@keyframes turn-meter \{([\s\S]*?)\n\}/);
    expect(match).not.toBeNull();
    const declarations = keyframeDeclarations(match![1]);
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^transform\s*:/);
    }

    // Negative control (as first written): an opacity fade, both declarations `;`-terminated.
    const fade = "\n  from { transform: scaleX(0.25); opacity: 0.4; }\n  to { transform: scaleX(1); opacity: 1; }\n";
    expect(keyframeDeclarations(fade).some((d) => !d.startsWith("transform:"))).toBe(true);

    // Negative control (c), the REPRODUCED bypass, in the reviewer's own exact shape: a final
    // declaration with no trailing `;`, which is how a last declaration is most often written.
    // The second `expect` is the point of this control -- it runs the OLD pattern over the same
    // string and shows it finds nothing wrong, so this is a hole that was really open rather than
    // one this test merely asserts about.
    const unterminated = "\n  from { transform: scaleX(0.25); }\n  to { transform: scaleX(1); opacity: 1 }\n";
    expect(keyframeDeclarations(unterminated).filter((d) => !d.startsWith("transform:"))).toEqual(["opacity: 1"]);
    expect((unterminated.match(/[a-z-]+:[^;]*;/g) ?? []).filter((d) => !d.startsWith("transform:"))).toEqual([]);
  });
});

/**
 * Which declaration WINS, not merely which ones exist.
 *
 * Every other test in this file is a regex over rule TEXT, and that is a real blind spot rather
 * than a stylistic one: CSS specificity silently defeated a planned rule three separate times on
 * this branch, and all three are recorded in `index.css` as prose -- `.row-assistant pre` (0,1,1)
 * beating `.code-block` (0,1,0) and winning the code block's background back to `--nv-bg`;
 * `.mode-selector button` (0,1,1) beating `.row`/`.row-choice` (0,1,0) and putting the boxed
 * `display: block`/`padding: 10px` back over the sign-column grid; and the old
 * `.mode-selector .session-choice button.selected` outweighing `.row-choice.selected`. Prose is
 * memory, not a guard: a fourth loss would ship green exactly as the first three would have.
 *
 * So: attach the real stylesheet to a real DOM and read `getComputedStyle`. jsdom resolves class
 * and descendant selectors and applies specificity for ordinary (non-`var()`) declarations, which
 * covers all three named cases below -- each with its own test and its own negative control that
 * reintroduces the exact rule that used to win. The "boxed look" case gets TWO tests, not one: it
 * is two separate CSS rules (index.css:46 for `display`/`padding`, index.css:470 for
 * `border`/`background`) that can regress independently, even though the prose paragraph above
 * narrates them as one incident.
 * (Correction, this same review round: this comment used to claim "covers all three cases" while
 * only two of the three were actually exercised -- index.css:470's rule and the third, `.selected`,
 * case had no assertion and no fixture element to read them from. Both are covered now; see the
 * two tests below the "boxed-button padding" one. Closing that gap surfaced the limitation below
 * that the comment did not know about yet, and forced index.css:470's test onto a property this
 * comment did not originally plan to use.)
 *
 * THREE THINGS IT CANNOT DO, said plainly so nobody reads more into a green run than is there:
 *   - jsdom does not resolve `var()`. A winning `background: var(--nv-surface)` computes to the
 *     literal string "var(--nv-surface)". That is exactly what makes an assertion on such a
 *     property useful when nothing else competes for it -- the string names the token, so a
 *     different winner reads as a different string -- but it is a comparison of declarations, not
 *     of rendered colour.
 *   - jsdom's CSS engine fails to parse a SHORTHAND containing `var()`, so `border-left: 3px solid
 *     var(--nv-mode-browse)` sets no longhand at all here, and neither does index.css:470's
 *     `border: 1px solid var(--nv-border)` -- probed directly: a plain `.row-choice` (`border:
 *     none`) and a `.row-choice` under the reintroduced boxed-look rule come back with IDENTICAL
 *     `border`/`borderStyle`/`borderWidth`/`borderColor`. The mode block's BROWSE half is
 *     unobservable for the same reason; its INPUT half is asserted because its override is the
 *     `border-left-color` LONGHAND, which parses.
 *   - **A HIGHER-specificity `var()` value losing to a LOWER-specificity plain-literal value is
 *     decided by source order here, not by specificity -- discovered while closing the coverage
 *     gap above, not assumed.** Mapped with a 2x2 probe (higher/lower specificity x which side
 *     holds `var()`), not just the one pair that broke index.css:470's test: when BOTH competing
 *     values are `var()`, or BOTH are plain literals, this engine picks the correct
 *     higher-specificity winner regardless of order (matches real cascade behaviour). The single
 *     broken combination is higher-specificity-`var()` vs. lower-specificity-literal: the literal
 *     wins whenever it is LAST in the stylesheet, the `var()` side wins whenever IT is last --
 *     source order, not specificity, and the lower-specificity side can win outright. The reverse
 *     pairing (higher-specificity literal vs. lower-specificity `var()`) is unaffected: the
 *     literal, being both higher-specificity and a plain value, wins regardless of order.
 *     index.css:470 is exactly the broken combination: `.mode-selector button`'s `background:
 *     var(--nv-bg)` (0,1,1) against `.row-choice`'s `background: none` (0,1,0), with `.row-choice`
 *     sitting AFTER it in the real file. So `.row-choice` keeps "winning" `background` by order
 *     whether or not `:not(.row-choice)` is there to make it lose by specificity -- reintroducing
 *     that exact historical bug in the real `index.css` and re-running this file leaves
 *     `.row-choice`'s `background` unchanged at `"rgba(0, 0, 0, 0)"` either way, so an assertion on
 *     `background` there would not have guarded anything (this was tried and confirmed against the
 *     real file, not skipped). index.css:470's test below asserts `border-radius` instead: it
 *     carries no `var()` and nothing on `.row-choice` competes for it at all, so it is decided by
 *     ordinary specificity and genuinely flips when the bug is reintroduced.
 *     The code-block and `.selected` tests are unaffected: both are the OTHER, unbroken
 *     combination -- `.row-assistant pre` vs. `.code-block`, and the old
 *     `.session-choice button.selected` vs. `.row-choice.selected`, are each `var()` on BOTH
 *     sides, which this engine resolves by genuine specificity regardless of where either rule
 *     sits (checked directly: moving the reintroduced `.row-assistant pre` rule to its real,
 *     original location earlier in the file, instead of appending it, does not change which one
 *     wins). Their `background` assertions are real specificity checks, not order coincidences.
 * Neither is a claim about a browser. Real rendering is still `shell/MANUAL_VERIFICATION.md`'s.
 */
describe("index.css cascade (which rule actually wins)", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  /** Mounts `extraCss` after the real stylesheet and returns the element matching `selector`. */
  function computed(html: string, selector: string, extraCss = ""): CSSStyleDeclaration {
    document.head.innerHTML = `<style>${css}${extraCss}</style>`;
    document.body.innerHTML = html;
    return getComputedStyle(document.body.querySelector(selector)!);
  }

  const CODE_BLOCK = `<div class="message-list"><div class="row row-assistant"><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
  // Two buttons, not one: the plain row exercises index.css:470's `border`/`background` guard
  // below, and `.selected` exercises `.row-choice.selected`'s own fill -- neither was reachable
  // when this fixture held only the unselected row.
  const CHOICE_ROW = `<div class="mode-selector"><div class="session-choice"><button type="button" class="row row-choice"><span class="row-sign">›</span><span class="row-body">y</span></button><button type="button" class="row row-choice selected"><span class="row-sign">›</span><span class="row-body">z</span></button></div></div>`;
  const MODE_BLOCK = `<div class="status-line"><span class="mode-block" data-mode="input">INPUT</span></div>`;
  const UNFOCUSED_INPUT_BLOCK = `<div class="status-line"><span class="mode-block" data-mode="input" data-focused="false">INPUT</span></div>`;

  // Review (2026-09-19): the conversation used to be `grid-template-rows: auto 1fr auto auto`, which
  // gives the `1fr` to the SECOND child -- the fatal-error banner when it is shown, not the list.
  // The list then took its full content height and never scrolled, so `j`/`k` could not step
  // through a long reply over a dead session. jsdom has no layout, so this pins the rule that
  // decides it: the list grows by its own class, whatever sits between it and the winbar.
  const CONVERSATION = (banner: string) =>
    `<div class="agent-ui-root agent-ui-conversation"><div class="winbar">w</div>${banner}<div class="agent-ui-scroller"><div class="message-list">m</div></div><div class="status-line">s</div><div class="composer">c</div></div>`;

  it("gives the free height to the message list, fatal banner or not", () => {
    for (const banner of ["", `<div class="fatal-error">e</div>`]) {
      const list = computed(CONVERSATION(banner), ".message-list");
      expect(list.display).not.toBe("grid");
      expect(list.flexGrow).toBe("1");
      expect(list.minHeight).toBe("0px");
      expect(list.overflowY).toBe("auto");
      // The chain, not just its last link: the free height reaches the list through the scroller
      // the `?` overlay is positioned against, so both have to grow and neither may keep a floor.
      const scroller = computed(CONVERSATION(banner), ".agent-ui-scroller");
      expect(scroller.flexGrow).toBe("1");
      expect(scroller.minHeight).toBe("0px");
      const root = computed(CONVERSATION(banner), ".agent-ui-conversation");
      expect(root.display).toBe("flex");
      expect(root.gridTemplateRows).toBe("none");
      for (const other of [".winbar", ".status-line", ".composer", ...(banner ? [".fatal-error"] : [])]) {
        expect(computed(CONVERSATION(banner), other).flexGrow).toBe("0");
      }
    }
    // Negative control: the old grid put back, which is what hands the space to the banner.
    const old = ".agent-ui-conversation { display: grid; grid-template-rows: auto 1fr auto auto; }";
    expect(computed(CONVERSATION(`<div class="fatal-error">e</div>`), ".agent-ui-conversation", old).display).toBe("grid");
  });

  /// The `?` overlay covers the conversation and nothing else. Its box comes from its containing
  /// block, so the pair that decides it -- the scroller being positioned, the overlay being
  /// absolute -- is the invariant, not the overlay's own rule alone. The first version covered the
  /// whole panel and put its own first heading under the winbar, unreadable and unreachable.
  it("keeps the ? overlay inside the list's own region, not over the two bars", () => {
    const markup =
      `<div class="agent-ui-root agent-ui-conversation"><div class="winbar">w</div>` +
      `<div class="agent-ui-scroller"><div class="message-list">m</div>` +
      `<div class="keymap-overlay"><section><h2>This panel</h2></section></div></div>` +
      `<div class="status-line">s</div></div>`;
    expect(computed(markup, ".agent-ui-scroller").position).toBe("relative");
    expect(computed(markup, ".keymap-overlay").position).toBe("absolute");
    // And the bars are back to taking part in normal painting: nothing has to out-stack the overlay.
    for (const bar of [".winbar", ".status-line"]) {
      expect(computed(markup, bar).zIndex).toBe("auto");
    }
  });

  it("does not dim the which-key strip with opacity", () => {
    // The third instance of the same defect this file has now recorded twice (picker rows, winbar):
    // opacity multiplies a guarded pair's contrast down with it. `? keys` recedes by token instead.
    const rules = withoutComments.match(/\.which-key[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("paints a code block on --nv-surface even inside an assistant row", () => {
    expect(computed(CODE_BLOCK, "pre").background).toBe("var(--nv-surface)");
    // The negative control: the rule that was actually deleted, put back. Without it this
    // assertion could be passing because nothing competes, which is not the same as winning.
    expect(computed(CODE_BLOCK, "pre", ".row-assistant pre { background: var(--nv-bg); }").background).toBe(
      "var(--nv-bg)",
    );
  });

  /* --- Task 1 (2026-09-20): wide content escapes the 62ch prose measure -----------------------
     The owner's report: fullscreen leaves a big empty strip down the right of the chat panel,
     because a fenced code block, the permission card's diff and a tool result all wrapped at the
     same 62ch the `.row` grid caps PROSE at (`index.css:136`'s `minmax(0, 62ch)`).

     jsdom runs no real Grid layout (see the big comment atop this describe block), so none of
     these tests can see an actual resolved pixel width -- what they CAN see, and what the fix
     actually is, is a `cqw`-based `width` declaration reaching a specific element and not others.
     A `cqw` unit is relative to `.row`'s own `container-type: inline-size` (also new), which is
     what lets it reach `.tool-result-body`/`.permission-card-edit` through several PLAIN wrapper
     elements (`.tool-call`, `[data-awaiting-permission]`, `.permission-card`) that a subgrid
     alternative could not reach without giving up their own box (background/border/padding). */
  it("lets a fenced code block ignore the 62ch measure, unlike a paragraph in the very same row", () => {
    // Real defect reproduced: before this task, `.code-block` had no `width` rule at all, so it
    // was exactly as capped by `.row-body`'s grid track as the paragraph beside it.
    const html =
      `<div class="message-list"><div class="row row-assistant"><div class="row-body">` +
      `<p>prose</p><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const prose = computed(html, "p");
    const code = computed(html, "pre.code-block");
    expect(code.width).toMatch(/cqw/);
    expect(prose.width).not.toMatch(/cqw/);
    // Negative control: without the rule this task adds, a code block is exactly as capped as the
    // paragraph next to it -- there is nothing else in this file that would widen it.
    expect(computed(html, "pre.code-block", ".code-block { width: auto; }").width).not.toMatch(/cqw/);
  });

  it("lets a tool result ignore the 62ch measure through two plain, unstyled wrapper elements", () => {
    // `.tool-result-body` sits inside `[data-awaiting-permission]` > `.tool-call` >
    // `.tool-result` -- MessageList.tsx and toolRegistry.tsx's real nesting -- and NONE of those
    // three carries a rule in this file. A fix that (wrongly) targeted `.row-body` itself, rather
    // than the leaf, would not be exercised by a fixture this shallow; this one is exactly as deep
    // as the real DOM to make sure the `cqw` unit really does reach through, not just past a
    // fixture shortcut.
    const html =
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div data-awaiting-permission><div class="tool-call"><div class="tool-result">` +
      `<pre class="tool-result-body">out</pre></div></div></div></div></div></div>`;
    expect(computed(html, "pre.tool-result-body").width).toMatch(/cqw/);
    expect(computed(html, "pre.tool-result-body", ".tool-result-body { width: auto; }").width).not.toMatch(/cqw/);
  });

  it("widens the permission-card diff's own bordered box, not just the text inside it", () => {
    // `.permission-card-edit` sets `overflow: hidden` -- widening `.permission-card-diff`/
    // `.diff-line` INSIDE it without widening this box would just be clipped at its edge, so this
    // is the element the fix actually has to touch, and the one the sign-column-x-position test
    // below does not otherwise cover.
    const html =
      `<div class="permission-card"><div class="permission-card-edit"><div class="permission-card-edit-head">h</div>` +
      `<pre class="permission-card-diff"><div class="diff-line diff-added"><span class="diff-gutter">+</span></div></pre></div></div>`;
    const edit = computed(html, ".permission-card-edit");
    expect(edit.width).toMatch(/cqw/);
    // It also has to climb back out of `.permission-card`'s own box first, or the widened box
    // would start further right than a code block or a tool result does. **-11px, not -10px**
    // (whole-branch review, 2026-09-20): with the global `box-sizing: border-box` the card's
    // content box begins `border-left` + `padding-left` inside, and this test used to pin the
    // padding alone. The padding half is read back below; the 1px border is NOT readable here --
    // `border: 1px solid var(--nv-border)` is a shorthand containing `var()`, which this engine
    // drops entirely (see the big comment atop this describe block) -- so the number is checked
    // against the one half jsdom can see plus this sentence, rather than being pinned blind.
    expect(computed(html, ".permission-card").paddingLeft).toBe("10px");
    expect(edit.marginLeft).toBe("-11px");
    expect(computed(html, ".permission-card-edit", ".permission-card-edit { width: auto; margin-left: 0; }").width).not.toMatch(
      /cqw/,
    );
  });

  /* Whole-branch review (2026-09-20): the raw-JSON `<pre>` is the FIFTH element under the 62ch cap
     and the one the product draws most, since `editPreview` returns null for `Bash`/`WebFetch`/an
     unknown tool -- which per CLAUDE.md's permission-policy row is nearly every card. It is the
     same class of miss the review already caught once for `.tool-card-generic`. */
  it("lets a permission card's raw-JSON input ignore the 62ch measure, with the card's own inset backed out", () => {
    const html =
      `<div class="message-list"><div class="row row-permission"><div class="row-body">` +
      `<div class="permission-card"><pre class="permission-card-input">{"command":"..."}</pre></div>` +
      `</div></div></div>`;
    const input = computed(html, ".permission-card-input");
    expect(input.width).toMatch(/cqw/);
    // The same -11px as the diff box above, and for the same reason: both sit inside
    // `.permission-card`'s border + padding, so both have to climb back out by the same amount or
    // the two would not line up with each other, let alone with a code block.
    expect(input.marginLeft).toBe("-11px");
    expect(input.marginLeft).toBe(computed(html, ".permission-card-input").marginLeft);
    // Negative control: without this rule it is exactly as capped as it was.
    expect(computed(html, ".permission-card-input", ".permission-card-input { width: auto; margin-left: 0; }").width).not.toMatch(
      /cqw/,
    );
  });

  /* Whole-branch review (2026-09-20): `calc(100cqw - 30px)` assumes `.tool-result-body`'s parent is
     flush with `.row-body`. On the FAILED-tool path it is not -- `.tool-result-error` puts a 2px
     border and 6px of padding on that exact parent -- so the body overhung `.row`'s right edge by
     8px. The original fixture used a plain `.tool-result`, so neither the prose nor the suite
     covered the one state where the rule was untrue. */
  it("narrows a tool result by its error gutter, so a FAILED tool lands on the same right edge as a passing one", () => {
    const row = (errorClass: string) =>
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div class="tool-call"><div class="tool-result${errorClass}">` +
      `<pre class="tool-result-body">out</pre></div></div></div></div></div>`;
    // jsdom normalises a `calc()` into `calc(<length> + <length>)` order, so these read the
    // subtrahend rather than the literal text this file spells.
    expect(computed(row(""), "pre.tool-result-body").width).toBe("calc(-30px + 100cqw)");
    // 2px border-left + 6px padding-left = 8px, so 38px, and the two right edges coincide.
    expect(computed(row(" tool-result-error"), "pre.tool-result-body").width).toBe("calc(-38px + 100cqw)");
    // Negative control: this is a real specificity win, not an accident of the fixture. Deleting
    // the narrower rule (simulated by re-declaring the base one after it, at higher specificity)
    // puts the overhanging width back.
    expect(
      computed(row(" tool-result-error"), "pre.tool-result-body", ".tool-result-error .tool-result-body { width: calc(100cqw - 30px); }")
        .width,
    ).toBe("calc(-30px + 100cqw)");
  });

  /* --- the in-flight indicator's own layout (whole-branch review, 2026-09-20) ------------------
     Two findings, one bar. jsdom runs no flex layout, so both tests read the DECLARATIONS that
     decide the outcome -- which is the same thing every other test in this block does, and is
     exactly why the defects were invisible until someone did the flexbox arithmetic by hand. */

  it("keeps the meter from being the first thing squeezed off a narrow status line", () => {
    // `.meter`'s only child is an EMPTY span, so its min-content size is 0 and flexbox's automatic
    // minimum (§4.5) is min(4ch, 0) = 0. As an ordinary flex item it shrank to nothing -- the one
    // animated element in the product silently gone on the narrow panel where it matters most.
    const html = `<div class="status-line"><span class="turn-activity" data-phase="thinking"><span class="meter"><span class="meter-fill"></span></span><span class="turn-state">thinking</span></span></div>`;
    const meter = computed(html, ".meter");
    expect(meter.flexShrink).toBe("0");
    expect(meter.flexGrow).toBe("0");
    // `4ch` is pinned through this engine's own resolution of it rather than as text: it computes
    // `ch` at 0.5em, and `.status-line` sets `font-size: 12px`, so four of them is 24px. Any other
    // width fails here. (That is a jsdom metric, not a real font's -- the number a real WebKitGTK
    // resolves is a screen question, and design §12 asks a human whether 4ch is visible at all.)
    expect(meter.width).toBe("24px");
    // The same idiom, on the element this file already used it on -- so the fix is the file's own
    // answer rather than a new one.
    const gutter = computed(`<div class="diff-line"><span class="diff-gutter">+</span></div>`, ".diff-gutter");
    expect(gutter.flexShrink).toBe("0");
    // Negative control: the default `flex-shrink: 1` put back is the defect, and it is reachable
    // from any later rule that touches `flex` on this element.
    expect(computed(html, ".meter", ".turn-activity .meter { flex: 1 1 auto; }").flexShrink).toBe("1");
  });

  it("spends the phase word before the Stop button as the status line narrows", () => {
    // The priority order index.css states in prose, asserted as the TWO independent flex
    // distributions it really is: the indicator's box gives before the status word and the
    // position counter (its siblings in `.status-line`), and inside that box the phase word is the
    // only thing that can give. The meter, the clock, the mode block and Stop are never spent by
    // either. The Stop button is the one that matters -- App.tsx's own comment says it is the ONLY
    // Stop control for mouse users, so it going off the right edge is the interrupt affordance
    // leaving the screen.
    const html =
      `<div class="status-line">` +
      `<span class="mode-block" data-mode="browse">BROWSE</span>` +
      `<span class="status status-running">working</span>` +
      `<span class="turn-activity" data-phase="tool"><span class="meter"><span class="meter-fill"></span></span>` +
      `<span class="turn-state">running NotebookEdit</span><span class="turn-elapsed">123s+</span></span>` +
      `<span class="position">12/34</span>` +
      `<button type="button" class="stop">Stop</button></div>`;
    const shrink = (selector: string) => Number(computed(html, selector).flexShrink);

    // **The re-review of 2026-09-20 found the assertion that used to stand here unable to fail.**
    // It compared `.turn-state` against `.status` -- items in DIFFERENT flex containers, which
    // never compete in one distribution, so no arrangement of those two numbers could have
    // implemented or broken the order the comment claimed. The structure is asserted first now, so
    // that reading is impossible to make again by eye.
    const dom = new DOMParser().parseFromString(html, "text/html");
    expect(dom.querySelector(".turn-state")?.parentElement?.className).toBe("turn-activity");
    expect(dom.querySelector(".status")?.parentElement?.className).toBe("status-line");
    expect(dom.querySelector(".position")?.parentElement?.className).toBe("status-line");

    // Decision 1, among the children of `.status-line` that can give at all: the INDICATOR'S BOX
    // first, by a wide margin -- shrinking is distributed by factor x base size, so these numbers
    // are an order and not a ratio -- then the status word, then the position counter.
    expect(shrink(".turn-activity")).toBeGreaterThan(shrink(".status"));
    expect(shrink(".status")).toBeGreaterThan(shrink(".position"));
    expect(shrink(".position")).toBeGreaterThan(0);

    // Decision 2, inside `.turn-activity`: the phase word is the ONLY item there that can give, so
    // decision 1 spending that box IS the phase word truncating. This is the step that turns the
    // outer order into the documented one, and it holds because of what is zero here, not because
    // of how large `.turn-state`'s own factor is -- which is why that factor is now the plain
    // default rather than a number implying a rank it cannot express.
    expect(shrink(".meter")).toBe(0);
    expect(shrink(".turn-elapsed")).toBe(0);
    expect(shrink(".turn-state")).toBeGreaterThan(0);

    // ...and what `flex: none` on the meter and the clock actually buys: the DISTRIBUTION never
    // spends them. It does not mean they can never be clipped -- once the word is gone there is
    // nothing left inside the box to give -- so `.turn-activity` clips its own overflow rather
    // than letting the meter and the clock run out over `.position`.
    expect(computed(html, ".turn-activity").overflow).toBe("hidden");

    // Never spent by either distribution: the two fixed parts of the indicator, the mode block,
    // and the control that stops the turn.
    for (const fixed of [".meter", ".turn-elapsed", ".mode-block", ".stop"]) {
      expect(shrink(fixed)).toBe(0);
    }
    // A word that shrinks has to be ABLE to: its automatic minimum is its min-content size (the
    // whole word) unless `min-width: 0` says otherwise, and `text-overflow` never engages without
    // an `overflow` that is not `visible`.
    for (const shrinkable of [".turn-state", ".status", ".position"]) {
      expect(computed(html, shrinkable).minWidth).toBe("0px");
      expect(computed(html, shrinkable).overflow).toBe("hidden");
      expect(computed(html, shrinkable).textOverflow).toBe("ellipsis");
    }
    // `.turn-activity` is itself a flex ITEM: its child cannot shrink unless it can.
    expect(shrink(".turn-activity")).toBeGreaterThan(0);
    expect(computed(html, ".turn-activity").minWidth).toBe("0px");
    // And the bar never becomes two lines. `.winbar` wraps for this, deliberately not copied here:
    // line breaking happens on base sizes BEFORE shrinking, so a wrapping status line would jump
    // between one and two rows every time the phase word changed width.
    expect(computed(html, ".status-line").flexWrap).not.toBe("wrap");
    expect(computed(html, ".status-line").whiteSpace).toBe("nowrap");
    expect(computed(`<div class="winbar">w</div>`, ".winbar").flexWrap).toBe("wrap");
    // Negative control: the state this replaced -- nothing shrinking, nothing clipping -- is what
    // pushed Stop off the edge, and it is one later `flex` declaration away.
    expect(computed(html, ".turn-state", ".status-line .turn-activity .turn-state { flex: none; }").flexShrink).toBe("0");
  });

  /* Review of Task 1: an unrecognized tool's raw JSON dump is a fourth `<pre>` under the same
     62ch-capped `.row-body` and had been missed the first time this list was written. Widens the
     SAME element the fenced-code-block test above does (the outer box, not the bare `<pre>` inside
     it) -- `.tool-card-generic` needs no compensating `margin-left`, unlike `.permission-card-edit`
     above, because nothing between it and `.row-body` (`.tool-call`, `[data-awaiting-permission]`)
     carries a rule in this file either. */
  it("lets an unrecognized tool's raw JSON dump ignore the 62ch measure too", () => {
    const html =
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div data-awaiting-permission><div class="tool-call">` +
      `<details class="tool-card tool-card-generic"><summary>s</summary><pre>{}</pre></details>` +
      `</div></div></div></div></div>`;
    const card = computed(html, ".tool-card-generic");
    expect(card.width).toMatch(/cqw/);
    expect(card.marginLeft).not.toBe("-10px");
    // Negative control: without the rule this fix adds, it is exactly as capped as everything else
    // in `.row-body`.
    expect(computed(html, ".tool-card-generic", ".tool-card-generic { width: auto; }").width).not.toMatch(/cqw/);
  });

  it("keeps the sign column on the same track whether the row's body is prose or a wide code block", () => {
    // The entire reason the grid exists (`index.css:136-150`'s comment): the sign column must sit
    // at the same x position down the whole page. Widening the BODY column must never be able to
    // move it, so this compares `.row`'s own `grid-template-columns` -- the declaration that
    // decides column 1's width -- across a plain-prose row and a row whose body is a wide code
    // block, and pins it to the literal value so a future change to column 1 fails here too.
    const proseRow =
      `<div class="message-list"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><p>prose</p></div></div></div>`;
    const codeRow =
      `<div class="message-list"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const proseColumns = computed(proseRow, ".row").gridTemplateColumns;
    expect(proseColumns).toBe("22px minmax(0, 62ch)");
    expect(computed(codeRow, ".row").gridTemplateColumns).toBe(proseColumns);
    // Negative control: proves the assertion above can actually fail. A rule that changed column 1
    // wins the SAME assertion path used above, at the same specificity as `.row`'s own base rule
    // (`.row-assistant`, 0,1,0), placed after it -- reproducing the shape a careless edit to this
    // grid, or to a class every row already carries, would take.
    expect(computed(codeRow, ".row", ".row-assistant { grid-template-columns: 24px minmax(0, 62ch); }").gridTemplateColumns).not.toBe(
      proseColumns,
    );
  });

  it("gives `.row` a size query container without disturbing its own grid tracks", () => {
    // The mechanism the four tests above all rely on: `container-type: inline-size` has to be on
    // by the time any of them run, or every `cqw`-based width above would be checking a unit that
    // resolves against nothing. Pinned on its own so a regression here explains itself instead of
    // surfacing as four unrelated-looking failures above.
    const row = computed(CODE_BLOCK, ".row");
    expect(row.getPropertyValue("container-type")).toBe("inline-size");
    expect(row.display).toBe("grid");
    expect(row.gridTemplateColumns).toBe("22px minmax(0, 62ch)");
  });

  it("leaves a choice row on the sign-column grid, not on the boxed-button padding", () => {
    const row = computed(CHOICE_ROW, "button");
    expect(row.display).toBe("grid");
    expect(row.gridTemplateColumns).toBe("22px minmax(0, 62ch)");
    // `.mode-selector button:not(.row-choice)`'s 10px would show up here; `.row`'s own is `2px 0`
    // and `.row-choice` narrows it to `4px 0`, so the left padding is the tell.
    expect(row.paddingLeft).toBe("0px");
    const clobbered = computed(CHOICE_ROW, "button", ".mode-selector button { display: block; padding: 10px; }");
    expect(clobbered.display).toBe("block");
    expect(clobbered.paddingLeft).toBe("10px");
  });

  it("keeps a choice row off the boxed-button look", () => {
    // The line-470 sibling of the line-46 `:not(.row-choice)` guard above -- same specificity
    // reasoning (`.mode-selector button` still beats `.row-choice` by the type selector alone),
    // same historical shape, but a SEPARATE rule (border/background, the boxed look, rather than
    // display/padding) that can regress independently and was not caught by the test above.
    //
    // `borderRadius` is the property asserted, not `background` or `border`, and that choice was
    // forced by a real, separately-verified limitation (full matrix in the big comment above this
    // describe block): this rule's `background: var(--nv-bg)` (0,1,1) against `.row-choice`'s
    // `background: none` (0,1,0) is exactly the ONE combination -- higher-specificity `var()` vs.
    // lower-specificity plain literal -- this jsdom's CSS engine resolves by SOURCE ORDER rather
    // than specificity. `.row-choice`'s rule sits AFTER this one in the real file, so it keeps
    // "winning" `background` by order whether or not `:not(.row-choice)` is there to make it lose
    // by specificity -- reintroducing this exact bug in the real `index.css` and re-running this
    // file leaves `background` unchanged at `"rgba(0, 0, 0, 0)"` in both cases, so an assertion on
    // `background` here would silently not guard anything. `border` is separately unobservable
    // (the shorthand-plus-`var()` limitation documented above). `border-radius: 4px`, by contrast,
    // has no `var()` and nothing on `.row-choice` competes for it at all, so it is decided by
    // ordinary specificity: unset while `:not(.row-choice)` excludes the row, `4px` the moment
    // that exclusion is dropped -- confirmed by the same real-file mutation.
    const row = computed(CHOICE_ROW, "button.row-choice:not(.selected)");
    expect(row.borderRadius).toBe("");
    const clobbered = computed(
      CHOICE_ROW,
      "button.row-choice:not(.selected)",
      ".mode-selector button { border: 1px solid var(--nv-border); border-radius: 4px; background: var(--nv-bg); color: var(--nv-fg); cursor: pointer; }",
    );
    expect(clobbered.borderRadius).toBe("4px");
  });

  it("paints the selected choice row's own fill, not the old per-list selected rule's", () => {
    // The third historical loss the doc comment above names: `.mode-selector .session-choice
    // button.selected` (0,3,1) used to outweigh `.row-choice.selected` (0,2,0). Before this
    // fixture carried a `.selected` element, nothing in this file ever read this property.
    const row = computed(CHOICE_ROW, "button.selected");
    expect(row.background).toBe("var(--nv-cursorline)");
    const clobbered = computed(
      CHOICE_ROW,
      "button.selected",
      ".mode-selector .session-choice button.selected { background: var(--nv-bg); }",
    );
    expect(clobbered.background).toBe("var(--nv-bg)");
  });

  it("lets the per-mode border-left-color override the mode block's own border shorthand", () => {
    expect(computed(MODE_BLOCK, ".mode-block").borderLeftColor).toBe("var(--nv-mode-input)");
    const clobbered = computed(
      MODE_BLOCK,
      ".mode-block",
      ".status-line .mode-block { border-left: 3px solid var(--nv-mode-browse); }",
    );
    // A later rule at HIGHER specificity than the `[data-mode]` one would be the real defect; this
    // control is the weaker "later, equal-specificity shorthand", which is enough to show the
    // assertion above can fail. cssstyle drops a var() shorthand entirely, hence the empty string.
    expect(clobbered.borderLeftColor).not.toBe("var(--nv-mode-input)");
  });

  it("draws a focused button as the solid cursor, even a mode button whose own rule ties it", () => {
    // `.mode-selector button:not(.row-choice)` is (0,2,1), the same as the focus rule, so only
    // source order makes the focus rule win. The negative control re-declares it after.
    const html = `<div class="agent-ui-root mode-selector"><button type="button">Auto<span class="detail">d</span></button></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = html;
    const button = document.body.querySelector("button")!;
    button.focus();
    expect(getComputedStyle(button).background).toBe("var(--nv-fg)");
    expect(getComputedStyle(button).color).toBe("var(--nv-bg)");
    expect(getComputedStyle(button.querySelector(".detail")!).color).toBe("var(--nv-bg)");
    document.head.innerHTML = `<style>${css}.mode-selector button:not(.row-choice) { background: var(--nv-bg); }</style>`;
    expect(getComputedStyle(button).background).toBe("var(--nv-bg)");
  });

  it("steps the row cursor back to hollow while a control inside the row has focus", () => {
    const html = `<div class="message-list" data-focused="true"><div class="row row-permission row-current"><span class="row-sign">!</span><div class="row-body"><button type="button">Approve</button></div></div></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = html;
    expect(getComputedStyle(document.body.querySelector(".row-sign")!).background).toBe("var(--nv-fg)");
    // A fresh copy for the focused state: jsdom does not recompute an element's style when focus
    // moves, so reading the same element twice would return the first answer.
    document.body.innerHTML = html;
    document.body.querySelector("button")!.focus();
    const sign = document.body.querySelector<HTMLElement>(".row-sign")!;
    expect(getComputedStyle(sign).background).not.toBe("var(--nv-fg)");
    expect(getComputedStyle(sign).boxShadow).toBe("inset 0 0 0 1.5px var(--nv-fg)");
  });

  it("draws the panel's cursor solid with focus and hollow without", () => {
    const row = (focused: string) =>
      `<div class="message-list" data-focused="${focused}"><div class="row row-tool row-current"><span class="row-sign">⚙</span><div class="row-body">x</div></div></div>`;
    const solid = computed(row("true"), ".row-sign");
    expect(solid.background).toBe("var(--nv-fg)");
    expect(solid.color).toBe("var(--nv-bg)");
    expect(computed(row("true"), ".row").background).toBe("var(--nv-cursorline)");
    const hollow = computed(row("false"), ".row-sign");
    // jsdom resolves `background: none` to a transparent colour rather than echoing it, so this
    // asserts what matters: neither fill survives.
    expect(hollow.background).not.toBe("var(--nv-fg)");
    expect(hollow.color).toBe("var(--nv-fg)");
    expect(hollow.boxShadow).toBe("inset 0 0 0 1.5px var(--nv-fg)");
    expect(computed(row("false"), ".row").background).not.toBe("var(--nv-cursorline)");
    // Negative control: a solid-cursor rule at the unfocused rule's own specificity, placed after
    // it, wins the fill back. That is what moving the unfocused rules above the solid one, or
    // weakening their selector, would do.
    const clobbered = computed(
      row("false"),
      ".row-sign",
      '.message-list[data-focused="false"] .row-current .row-sign { background: var(--nv-fg); }',
    );
    expect(clobbered.background).toBe("var(--nv-fg)");
  });

  it("dims an unfocused mode block even in INPUT, whose own rule has equal specificity", () => {
    // `[data-mode="input"]` and `[data-focused="false"]` are both (0,3,0), so source order decides
    // and the unfocused rule must come second. Both sides are `var()` longhands, the combination
    // this engine resolves correctly (see the big comment above).
    const block = computed(UNFOCUSED_INPUT_BLOCK, ".mode-block");
    expect(block.borderLeftColor).toBe("var(--nv-chrome-muted)");
    expect(block.color).toBe("var(--nv-chrome-muted)");
    // A focused block keeps its per-mode rule and inherits the bright label colour.
    const focused = computed(MODE_BLOCK.replace('data-mode="input"', 'data-mode="input" data-focused="true"'), ".mode-block");
    expect(focused.borderLeftColor).toBe("var(--nv-mode-input)");
    expect(focused.color).not.toBe("var(--nv-chrome-muted)");
    // Negative control: the INPUT rule re-declared after the unfocused one wins it back. That is
    // what swapping the two rules in index.css would do.
    const clobbered = computed(
      UNFOCUSED_INPUT_BLOCK,
      ".mode-block",
      '.status-line .mode-block[data-mode="input"] { border-left-color: var(--nv-mode-input); }',
    );
    expect(clobbered.borderLeftColor).toBe("var(--nv-mode-input)");
  });
});

/**
 * Every `--nv-*` this stylesheet reads is a name Rust actually emits.
 *
 * A `var(--nv-typo)` is invisible twice over: CSS falls back to the property's initial value with
 * no error, and every other test in this file only checks the SHAPE of a declaration, never whether
 * the name inside it resolves. All 29 names in use were checked by hand on 2026-09-18; the next one
 * added would not have been.
 *
 * **The allowed set is DERIVED from `core/src/theme/tokens.rs`, never written down here.** A copy of
 * the list in this file would be a second thing to keep in sync with `ThemeTokens` -- the same
 * duplicated-definition defect this branch fixed for `isUsableLink` and for the row grid -- and it
 * would rot the first time a token was added, in the silent direction: the new name would look
 * unknown. So this reads that file's own text and reconstructs what `css_vars()` emits from the
 * three places it emits from. That makes this test sensitive to how `tokens.rs` is SPELLED, which
 * is the accepted cost and the reason the parse asserts its own sentinels before it is used: a
 * refactor that breaks the parse fails here loudly rather than quietly admitting everything.
 */
describe("--nv-* names", () => {
  /** What `ThemeTokens::css_vars` puts on the document, reconstructed from `tokens.rs`'s source. */
  const emitted = new Set<string>([
    // 1. the `colours` array in `css_vars`: `("chrome-fg", self.chrome_fg)` -> `--nv-chrome-fg`.
    ...Array.from(tokensRs.matchAll(/\("([a-z-]+)",\s*self\.[a-z_]+\)/g), (m) => `--nv-${m[1]}`),
    // 2. the `SYNTAX` table, emitted with a `--nv-syn-` prefix: `("keyword", "@keyword", ...)`.
    ...Array.from(tokensRs.matchAll(/\("([a-z_]+)",\s*"@/g), (m) => `--nv-syn-${m[1]}`),
    // 3. the handful pushed by literal name (the font stacks and the colour scheme).
    ...Array.from(tokensRs.matchAll(/"(--nv-[a-z-]+)"/g), (m) => m[1]),
  ]);

  it("parsed tokens.rs at all", () => {
    // Sentinels from each of the three sources above, so a regex that stopped matching fails here
    // rather than turning the real test below into "every name is allowed".
    for (const name of ["--nv-bg", "--nv-chrome-accent", "--nv-syn-keyword", "--nv-syn-tag", "--nv-font-mono", "--nv-color-scheme"]) {
      expect(emitted.has(name), `${name} missing -- the tokens.rs parse is broken, not the CSS`).toBe(true);
    }
    expect(emitted.size).toBeGreaterThanOrEqual(33);
    expect(emitted.has("--nv-not-a-real-token")).toBe(false);
  });

  it("is read only for names Rust emits", () => {
    const used = new Set(Array.from(withoutComments.matchAll(/var\((--nv-[a-z-]+)/g), (m) => m[1]));
    expect(used.size).toBeGreaterThan(20);
    expect(Array.from(used).filter((name) => !emitted.has(name))).toEqual([]);
  });
});
