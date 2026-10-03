// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, describe, expect, it } from "vitest";
import css from "./index.css?raw";
// The Rust side of the theme contract, read as TEXT at test time. See
// "every --nv-* this stylesheet reads is one Rust actually emits" at the bottom of this file for
// why the allowed names are derived from this source rather than written down here.
import tokensRs from "../../../core/src/theme/tokens.rs?raw";
// The pacer's threshold for telling the page that the user is typing is the meter's own step; the
// number lives in both languages, so this file holds them equal (see the typing-cadence test below).
import panelCadenceRs from "../../../core/src/panel_cadence.rs?raw";
import { NATURAL_METER_STEP_MS } from "./typingCadence";
// The sideways guard below renders the panel's real `<pre>`s (the GUI pass, 2026-09-24).
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { renderToolCall } from "./toolRegistry";
import { renderMarkdown } from "./markdown";
import { PermissionCard } from "./components/PermissionCard";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { WhichKeyBox } from "./components/WhichKeyBox";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { VISUAL_CHROME } from "./visual";

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
/** The solid cursor block: the current row's sign cell, a focused button with its children (every
 *  control is keyboard-reachable and the selected one is drawn as the cursor), the band's own
 *  `↓N` button (panel round 2 plan, Task 10; spec §5.2: "inverted", the same reversed body pair),
 *  the chooser's current row's sign cell (spec §6.1; r2-gui GUI pass, 2026-09-26), and CARET's own
 *  one-character block (visual-mode spec D3, revised for 3a: the same reversed body pair, only on
 *  the selection it paints itself). Nothing else. */
const CURSOR_BRANCH =
  /^(?:\.row-current \.row-sign|\.agent-ui-root button:focus(?: \*)?|\.band-unread|\.chooser-row\.current \.chooser-sign|\.message-list\[data-visual="caret"\] ::selection)$/;
/** The global `f` HINT's label, and nothing else (spec 2026-09-19-global-hint-design.md §2.3). */
const HINT_BRANCH = /^\.hint-label$/;
/** The which-key box's own keycap glyph (panel round 2 plan, Task 8), and nothing else -- a
 *  selector branch ending in exactly `.wk-key`. */
const WK_KEY_BRANCH = /(?:^|[\s.])wk-key$/;
/** The band's `⏵⏵` mode glyph (panel round 2 plan, Task 10; spec §9), and nothing else -- a
 *  selector branch that is `.mode-glyph` optionally followed by a `[data-mode-name="..."]`
 *  attribute selector, narrowed so it cannot widen to `.band-mode`/`.mode-pill`/anything else the
 *  glyph sits beside. */
const MODE_GLYPH_BRANCH = /(?:^|[\s.])mode-glyph(?:\[data-mode-name="[a-z]+"\])?$/;
/** A selector branch inside one of the boxes painted `--nv-surface` (sw-theme-2): the which-key box,
 *  a tool card, a permission card, the terminal handoff's command, a fenced code block. Where the
 *  text under such a selector actually lands is proven by rendering, in "text on a --nv-surface
 *  fill" at the bottom of this file; this only keeps the surface pair from being declared anywhere
 *  else. */
const SURFACE_BRANCH = /(?:^|[\s.])(?:which-key-box|tool-card(?:-[a-z]+)?|permission-card(?:-[a-z-]+)?|handoff-command|code-block)\b/;

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
 * the real guard test below for why that one is not a contrast exemption at all. Panel round 2 plan
 * Task 8 adds a fourth, narrower instance of that same non-guarantee: `--nv-syn-keyword` on exactly
 * the which-key box's own `.wk-key` glyph.
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
    const onWkKeySurface = everyBranchIsOnSurface(selector, WK_KEY_BRANCH);
    const onModeGlyphSurface = everyBranchIsOnSurface(selector, MODE_GLYPH_BRANCH);
    const onCardSurface = everyBranchIsOnSurface(selector, SURFACE_BRANCH);
    for (const declaration of body.match(/(?<![a-z-])color:[^;]*;/g) ?? []) {
      if (onHljsSurface && /^color: var\(--nv-syn-[a-z]+\);$/.test(declaration)) continue;
      if (onChromeSurface && /^color: var\(--nv-chrome-(fg|muted)\);$/.test(declaration)) continue;
      // Text on a card: `--nv-surface-fg`/`--nv-surface-muted`, guarded against `surface` in Rust
      // (sw-theme-2), and only inside the boxes that paint it.
      if (onCardSurface && /^color: var\(--nv-surface-(fg|muted)\);$/.test(declaration)) continue;
      // The which-key box's own keycap (panel round 2 plan, Task 8): the same deliberately-
      // unguarded exemption `--nv-syn-*` gets inside `hljs-*` above, extended to the one place
      // outside a code block that also wants a "this is a key" glyph -- narrowed to exactly
      // `.wk-key`, so it cannot widen to `.wk-group`/`.wk-disabled`/anything else in the box.
      if (onWkKeySurface && /^color: var\(--nv-syn-keyword\);$/.test(declaration)) continue;
      // The band's own mode glyph (panel round 2 plan, Task 10; spec §9): `--nv-warn`/`--nv-error`
      // as a glyph colour, not text -- `--nv-warn`/`--nv-error` are guarded at `UI_CONTRAST` (3.0)
      // against `bg`, not the 4.5:1 text needs, the same reasoning every other signal colour in
      // this file gets kept off text for.
      if (onModeGlyphSurface && /^color: var\(--nv-(warn|error)\);$/.test(declaration)) continue;
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

/** Every top-level `--custom-property: value;` declared inside a `:root { ... }` block, name (with
 *  its leading `--`) to raw declared-value TEXT. This is the one piece of `var()` this suite is
 *  allowed to resolve, and it is resolved from the stylesheet's own source rather than from
 *  anything jsdom computed: jsdom's `getComputedStyle` never substitutes `var()` at all (see the
 *  panel-width block below for the measurement that proves it), so `expandVars` below has to look
 *  the tokens up itself. A flat regex is enough because these declarations are not nested (the
 *  "contains no nested blocks today" test above is what keeps that assumption checked).
 *
 *  The `s` (dotAll) flag on the per-declaration regex is a fix, not decoration (round-2 review): a
 *  declaration whose VALUE spans multiple lines --
 *  `--bypass: calc(\n  100cqw\n  );`, exactly the shape `index.css`'s own real `margin-left: clamp(
 *  ...)` already uses across five lines for an ordinary property -- has a `\n` between the `:` and
 *  the terminating `;`, and `.` never matches `\n` without this flag, so `.+?` failed to match
 *  anything and the WHOLE declaration was silently dropped, token and all. That is a hole in this
 *  guard's own parsing, not an exotic input: reproduced directly by feeding it a real multi-line
 *  custom property and confirming it vanished from the returned map entirely. */
function rootCustomProperties(sheet: string): Map<string, string> {
  const tokens = new Map<string, string>();
  for (const block of sheet.match(/:root\s*\{[^}]*\}/g) ?? []) {
    const body = block.slice(block.indexOf("{") + 1, -1);
    for (const decl of body.split(";")) {
      const m = decl.match(/^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/s);
      if (m) tokens.set(m[1], m[2]);
    }
  }
  return tokens;
}

const ROOT_TOKENS = rootCustomProperties(withoutComments);
const ROW_TOKENS = rowCustomProperties(withoutComments);

/** Expands every `var(--name)` in `expr` against the real `:root` declarations above, recursively,
 *  so a chain like `--row-gutter` (itself built from two other tokens) fully unrolls into numbers.
 *  A test using this is still checking the real expression a browser would resolve -- just walked
 *  by hand against the stylesheet's own source, since neither jsdom nor this suite runs layout.
 *  Throws on a `var()` naming a token this file does not declare, rather than silently leaving it
 *  untouched, so a typo'd custom property name fails loudly instead of passing by accident. */
function expandVars(expr: string, depth = 0): string {
  if (depth > 10) throw new Error(`var() did not resolve after 10 rounds: ${expr}`);
  return expr.replace(/var\((--[\w-]+)\)/g, (_, name: string) => {
    const value = ROOT_TOKENS.get(name);
    if (value === undefined) throw new Error(`no :root declaration for ${name}`);
    return expandVars(value, depth + 1);
  });
}

/** Evaluates one `--fs-*` token's declared VALUE to a pixel number by doing the arithmetic, not by
 *  matching the fraction as text (round-2 review, "the text scale is one number and five ratios of
 *  it"). A string match on `"* 12 / 14"` cannot tell a correct ratio from a wrong one written with
 *  the same digits transposed, or from an equally-plausible different fraction landing on the same
 *  wrong pixel number by coincidence -- it only proves the FILE SAYS a particular fraction, never
 *  that the fraction is the right one. This instead extracts the two numbers `index.css` actually
 *  wrote (the fallback base and, for every token but `--fs-base` itself, the multiplier) and
 *  computes the real result, so a wrong ratio is caught by the number it produces rather than by
 *  its spelling. Throws on a value that is not one of the two literal shapes `--fs-*` tokens use
 *  today (`var(--nv-font-size, <n>px)` for `--fs-base`, `calc(var(--nv-font-size, <n>px) * <a> /
 *  <b>)` for every other one) -- a shape this test doesn't recognise should fail loudly, not be
 *  silently skipped. */
function evaluateFsExpression(raw: string): number {
  const plain = raw.match(/^var\(--nv-font-size,\s*([\d.]+)px\)$/);
  if (plain) return parseFloat(plain[1]);
  const calc = raw.match(/^calc\(var\(--nv-font-size,\s*([\d.]+)px\)\s*\*\s*([\d.]+)\s*\/\s*([\d.]+)\)$/);
  if (calc) return (parseFloat(calc[1]) * parseFloat(calc[2])) / parseFloat(calc[3]);
  throw new Error(`--fs-* value does not match either recognised shape: ${raw}`);
}

/** Every rule in `sheet` at least one branch of whose (possibly grouped) selector contains
 *  `needle` as a substring -- e.g. every `.row-prompt ...` rule, `.row-prompt` itself included.
 *  Flat, like `unguardedTextColorDeclarations` above, for the same reason: no nesting exists here
 *  today and that is itself a checked invariant. Kept separate from `everyBranchIsOnSurface`
 *  because the anti-bubble and spine checks below want EVERY matching branch inspected, not a
 *  "the whole group sits on one surface" all-or-nothing test. */
function rulesMatching(sheet: string, needle: string): { selector: string; body: string }[] {
  const rules = sheet.match(/[^{}]*\{[^{}]*\}/g) ?? [];
  const matches: { selector: string; body: string }[] = [];
  for (const rule of rules) {
    const selector = rule.slice(0, rule.indexOf("{"));
    if (selector.split(",").some((s) => s.includes(needle))) {
      matches.push({ selector, body: rule.slice(rule.indexOf("{") + 1, -1) });
    }
  }
  return matches;
}

/** Every `width` escape in `sheet`: a `width:` declaration that spends the row's own width,
 *  `var(--row-inline-size)`. Until 2026-09-24 these were `calc(100cqw ...)` expressions, found by
 *  paren counting; the escapes are whole declarations now, so a declaration split is enough. Used
 *  to prove every one of them spends `var(--row-gutter)` rather than its own copy of the sign
 *  column's width, and falls back to "no escape" (the panel-width invariants below). */
function rowEscapeWidths(sheet: string): string[] {
  const found: string[] = [];
  for (const rule of splitRules(stripComments(sheet))) {
    for (const raw of rule.declarations.split(";")) {
      const m = raw.match(/^\s*width\s*:\s*(.+?)\s*$/s);
      if (m !== null && m[1].includes("var(--row-inline-size)")) found.push(m[1]);
    }
  }
  return found;
}

/** Custom properties declared on exactly `.row` -- today only `--row-inline-size`, which has to be
 *  declared on the row rather than on `:root` because it resolves against the list's measured
 *  width, a value that exists only on `.message-list` (`MessageList.tsx` writes it). Read from the
 *  stylesheet's own source, like `ROOT_TOKENS`. */
function rowCustomProperties(sheet: string): Map<string, string> {
  const tokens = new Map<string, string>();
  for (const rule of splitRules(stripComments(sheet))) {
    if (rule.selector !== ".row") continue;
    for (const decl of rule.declarations.split(";")) {
      const m = decl.match(/^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/s);
      if (m) tokens.set(m[1], m[2]);
    }
  }
  return tokens;
}

/** `expandVars`, with `.row`'s own derived properties in scope as well as `:root`'s -- which is what
 *  a browser resolves for anything inside a row. `var(--list-inline-size, 0px)` is left as written:
 *  it is the one runtime input, the list's measured width, and carries its fallback inline. */
function expandRowVars(expr: string, depth = 0): string {
  if (depth > 10) throw new Error(`var() did not resolve after 10 rounds: ${expr}`);
  return expr.replace(/var\((--[\w-]+)\)/g, (_, name: string) => {
    const value = ROW_TOKENS.get(name) ?? ROOT_TOKENS.get(name);
    if (value === undefined) throw new Error(`no :root or .row declaration for ${name}`);
    return expandRowVars(value, depth + 1);
  });
}

/** Evaluates a length expression from this stylesheet to px, for a list whose content box is
 *  `listInlineSize` px wide (`null`: not measured yet, so the `0px` fallback applies) and an element
 *  whose containing block is `percentBasis` px wide. Enough CSS math for this file's own escapes
 *  and the prompt's inset -- `calc`, `max`, `min`, `clamp`, `+ - * /`, px, unitless and `%` -- and
 *  it throws on anything else rather than guess. jsdom lays nothing out, so this is how a test here
 *  can say "the escape lands on the row's right edge" as a number rather than as a spelling. */
function evaluatePx(expr: string, listInlineSize: number | null, percentBasis: number): number {
  const js = expandRowVars(expr)
    .replace(/var\(--list-inline-size,\s*0px\)/g, listInlineSize === null ? "0px" : `${listInlineSize}px`)
    .replace(/(\d+(?:\.\d+)?)%/g, (_, n: string) => `(${n} * ${percentBasis} / 100)`)
    .replace(/(\d+(?:\.\d+)?)px\b/g, "$1")
    .replace(/\bcalc\(/g, "(")
    .replace(/\bmax\(/g, "Math.max(")
    .replace(/\bmin\(/g, "Math.min(");
  const leftover = js.replace(/Math\.max|Math\.min|clamp/g, "");
  if (/[A-Za-z_$]/.test(leftover)) throw new Error(`cannot evaluate: ${js}`);
  const clamp = (lo: number, value: number, hi: number) => Math.max(lo, Math.min(value, hi));
  return new Function("clamp", `return (${js});`)(clamp) as number;
}

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
 *  rather than quietly yielding truncated blocks.
 *
 *  Hoisted to module scope (previously local to the `describe("index.css", ...)` callback) so the
 *  cascade describe block below and `allCustomPropertyDeclarations` can reuse the very same parser
 *  instead of each keeping a second copy that could drift out of sync with it. */
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

/** Every declaration in `sheet` that makes an element a size query container: `container-type` with
 *  any value but `normal`, or the `container` shorthand (whose second half is a `container-type`),
 *  at any nesting depth. Property names are matched case-insensitively, as CSS reads them. See the
 *  "declares no size query container" test for why this file must have none. */
function sizeContainerDeclarations(sheet: string): { selector: string; declaration: string }[] {
  const out: { selector: string; declaration: string }[] = [];
  for (const rule of splitRules(stripComments(sheet))) {
    for (const raw of rule.declarations.split(";")) {
      const declaration = raw.trim();
      const m = declaration.match(/^(container-type|container)\s*:\s*(.*)$/is);
      if (m === null) continue;
      if (m[1].toLowerCase() === "container-type" && m[2].trim().toLowerCase() === "normal") continue;
      out.push({ selector: rule.selector, declaration });
    }
  }
  return out;
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

/** Every `--custom-property: value;` declaration ANYWHERE in the sheet, at any nesting depth --
 *  unlike `rootCustomProperties`/`ROOT_TOKENS` above, which is intentionally a regex over
 *  unqualified, TOP-LEVEL `:root { ... }` blocks only (that is genuinely what `expandVars` needs:
 *  the one scope a real browser resolves `var()` against for this file's own declarations).
 *
 *  A regex anchored on the literal text `:root\s*\{` cannot see a custom property declared inside
 *  `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --foo: ...; } }` --
 *  there is a `:not(...)` between `:root` and `{`, so the regex never matches the block at all,
 *  and a custom property declared there is invisible to every guard built on `ROOT_TOKENS`. This
 *  walks the real, brace/quote-aware parse instead, so it finds a declaration wherever CSS itself
 *  would let one be written -- nested under `@media`, scoped by `:not()`/`:is()`, or anywhere else
 *  -- which is what a guard that must hold "for every custom property in the file" actually needs.
 *
 *  Same `s` (dotAll) fix as `rootCustomProperties` above, for the same reason: a declaration whose
 *  value wraps across lines has a literal `\n` between the `:` and the `;`, which `.` does not match
 *  by default, so the whole declaration -- name, value, container-query unit and all -- was silently
 *  invisible to the WebKitGTK guard this function backs. */
function allCustomPropertyDeclarations(sheet: string): { name: string; value: string; selector: string }[] {
  const out: { name: string; value: string; selector: string }[] = [];
  for (const rule of splitRules(stripComments(sheet))) {
    for (const decl of rule.declarations.split(";")) {
      const m = decl.match(/^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/s);
      if (m) out.push({ name: m[1], value: m[2], selector: rule.selector });
    }
  }
  return out;
}

/** Every `CSSStyleRule` in the sheet **including the ones nested inside `@media`, `@supports` and
 *  `@layer`**, in document order.
 *
 *  Both CSSOM walkers below used to iterate `sheet.cssRules` flat and `continue` past anything that
 *  was not a `CSSStyleRule`, which made every rule inside a grouping rule invisible to them. A
 *  reviewer walked straight through: a literal Cursor pill -- `background` + `border-radius: 12px`
 *  + `width: fit-content` on the real `.row-prompt .row-body` -- wrapped in
 *  `@media (min-width: 1px) { ... }` passed the whole suite green, and so did an `@media` block
 *  cancelling the prompt rule's border, padding and colour at once. `index.css` already has one
 *  `@media` block of its own, so this was not a hypothetical shape.
 *
 *  Grouping rules are flattened UNCONDITIONALLY, without asking whether their condition matches.
 *  That is deliberate and it is the safe direction for a guard: a rule that draws a bubble only at
 *  some widths, or only under `prefers-reduced-motion`, still draws a bubble. A guard that asked
 *  jsdom to evaluate the condition would go quiet exactly where the stylesheet got more
 *  complicated. */
function allStyleRules(rules: CSSRuleList): CSSStyleRule[] {
  const out: CSSStyleRule[] = [];
  for (const rule of Array.from(rules)) {
    if (rule instanceof CSSStyleRule) {
      out.push(rule);
      continue;
    }
    const nested = (rule as CSSGroupingRule).cssRules;
    if (nested) out.push(...allStyleRules(nested));
  }
  return out;
}

/** A rough CSS specificity as `[ids, classes/attrs/pseudo-classes, types/pseudo-elements]` --
 *  enough for this file's own selectors (plain classes, attribute selectors, `:not()`/`:focus`
 *  pseudo-classes counted at one each, type selectors) and validated below against specificities
 *  this file's own comments already state by hand (`.mode-selector button:not(.row-choice)` is
 *  documented as (0,2,1); `.band-mode[data-mode="input"]` as (0,2,0)). Not a full
 *  implementation of the spec's handling of `:not()`'s own argument, or of `:is()`/`:where()`,
 *  neither of which any selector in this file uses. */
function specificity(selector: string): [number, number, number] {
  const s = selector.trim();
  const ids = (s.match(/#[\w-]+/g) ?? []).length;
  const classAttrPseudo = (s.match(/\.[\w-]+|\[[^\]]*\]|:[\w-]+(?:\([^)]*\))?/g) ?? []).length;
  const stripped = s.replace(/#[\w-]+|\.[\w-]+|\[[^\]]*\]|::?[\w-]+(?:\([^)]*\))?/g, " ");
  const types = (stripped.match(/[a-zA-Z][\w-]*/g) ?? []).length;
  return [ids, classAttrPseudo, types];
}

function compareSpecificity(a: [number, number, number], b: [number, number, number]): number {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

/** A shorthand's own longhands, for the shorthands `winningDeclaration` below is actually asked to
 *  check. A rule that sets only `border-left-style`/`border-left-width` never sets the literal
 *  string `"border-left"` at all, so asking the CSSOM for that one property name is blind to it --
 *  the reviewer's own bypass, an equal-specificity, later `border-left-style: none; border-left-
 *  width: 0;` that cancels the real border while `winningDeclaration(..., "border-left")` kept
 *  reading the shorthand rule's own text and never noticed the longhands had moved past it. Not
 *  exhaustive -- extend it only when a new shorthand check needs one. */
const SHORTHAND_LONGHANDS: Record<string, string[]> = {
  "border-left": ["border-left-width", "border-left-style", "border-left-color"],
};

/** Whether `candidate` outranks `current` under the real cascade's own order: importance first
 *  (unconditionally -- an `!important` candidate always outranks a plain one, and a plain candidate
 *  never outranks an `!important` one, regardless of either one's specificity), THEN specificity,
 *  THEN source order at a tie. Split out of `winningDeclaration` so the three-way priority order is
 *  one small, directly readable function rather than a nested ternary. */
function beatsCurrentWinner(
  candidate: { important: boolean; spec: [number, number, number]; order: number },
  current: { important: boolean; spec: [number, number, number]; order: number },
): boolean {
  if (candidate.important !== current.important) return candidate.important;
  const cmp = compareSpecificity(candidate.spec, current.spec);
  if (cmp !== 0) return cmp > 0;
  return candidate.order >= current.order;
}

/** Which declaration for `property` actually wins on the element matching `elementSelector`, read
 *  through the browser's own selector-matching (`Element.matches`) over the REAL parsed CSSOM
 *  rules -- not a regex over rule text -- so a competing rule that legitimately outranks the one
 *  under test by specificity, or by later source order at equal specificity, is honoured exactly
 *  as a real cascade would honour it. Importance (`!important`) is honoured too, ahead of
 *  specificity and order both -- CSS decides importance FIRST, so a lower-specificity, EARLIER
 *  `!important` declaration still beats a higher-specificity, later plain one, and a version of
 *  this function that ignored priority entirely called that combination a false negative (the
 *  reviewer's bypass: `.row-body { border-left: none !important; }`, lower specificity than
 *  `.row-prompt .row-body`, appended at the end -- and it really does win in a browser).
 *
 *  If `property` is a known shorthand (`SHORTHAND_LONGHANDS`), a rule that sets one of ITS
 *  longhands competes in the very same race. When such a longhand declaration wins instead of the
 *  shorthand itself, the shorthand's own text is no longer what actually renders, and there is no
 *  general way from here to re-assemble "one shorthand plus a competing longhand" into a single
 *  value string -- so this returns `null` rather than a confident, wrong one. A caller checking a
 *  shorthand should therefore always assert what it means (`toBe(expected)`), never merely `not
 *  toBeNull()`, so a `null` this returns for the right reason cannot be mistaken for the win it is
 *  guarding against.
 *
 *  Needed at all because `getComputedStyle` cannot be used for `.row-prompt .row-body`'s
 *  `border-left`: it is a SHORTHAND carrying a `var()`, and jsdom's `cssstyle` drops any shorthand
 *  containing a `var()` entirely rather than resolving it (the same limitation the big comment atop
 *  the cascade describe block records for the mode block's BROWSE half) -- confirmed directly:
 *  mounting ONLY that rule and mounting NO rule at all produce byte-identical `getComputedStyle`
 *  output for every `border-left-*` longhand, so a regression that cancels the border cannot be
 *  seen through `getComputedStyle` at all, in either direction. A CSSOM RULE's own `.style`, unlike
 *  an element's resolved style, keeps the raw declared text (including any `var()`) -- confirmed
 *  directly too -- so this reads that instead, at the rule that priority+specificity+order actually
 *  pick. */
function winningDeclaration(html: string, elementSelector: string, property: string, extraCss = ""): string | null {
  document.head.innerHTML = `<style>${css}${extraCss}</style>`;
  document.body.innerHTML = html;
  const el = document.body.querySelector(elementSelector);
  if (el === null) throw new Error(`no element matches ${elementSelector}`);
  return winningDeclarationOn(el, property);
}

/** `winningDeclaration`'s cascade for an element already in the document, against the stylesheet
 *  already mounted as `document.styleSheets[0]` -- for a check over every element of a kind rather
 *  than the first one a selector finds (the sideways guard, the GUI pass of 2026-09-24). */
function winningDeclarationOn(el: Element, property: string): string | null {
  const sheet = document.styleSheets[0];
  const candidateProperties = [property, ...(SHORTHAND_LONGHANDS[property] ?? [])];
  let winner: { important: boolean; spec: [number, number, number]; order: number; prop: string; value: string } | null =
    null;
  let order = 0;
  for (const rule of allStyleRules(sheet.cssRules)) {
    order += 1;
    for (const branch of rule.selectorText.split(",").map((b) => b.trim())) {
      let matches = false;
      try {
        matches = el.matches(branch);
      } catch {
        matches = false;
      }
      if (!matches) continue;
      for (const prop of candidateProperties) {
        const value = rule.style.getPropertyValue(prop);
        if (!value) continue;
        const candidate = { important: rule.style.getPropertyPriority(prop) === "important", spec: specificity(branch), order };
        if (winner === null || beatsCurrentWinner(candidate, winner)) {
          winner = { ...candidate, prop, value };
        }
      }
    }
    order += 1;
  }
  return winner === null ? null : winner.prop === property ? winner.value : null;
}

describe("index.css", () => {
  it("contains no colour literal -- every colour comes from nvim through --nv-* variables", () => {
    expect(withoutComments.match(/#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(/g)).toBeNull();
  });

  /** Panel round 2 (plan Task 10; spec §9): `⏵⏵` (U+23F5) is not in FiraCode Nerd Font, so
   *  `.mode-glyph` names a real font stack rather than a `--nv-font-*` token -- the one named
   *  exemption to "both stacks come from Rust", narrowed to exactly that one selector and exactly
   *  that one literal stack so it cannot quietly widen to cover an unrelated rule. */
  const MODE_GLYPH_FONT_STACK =
    'font-family: "Noto Sans Symbols 2", "Symbola", "Segoe UI Symbol", "Adwaita Mono", var(--nv-font-mono);';

  it("names no font family -- both stacks come from Rust, except the mode glyph's own", () => {
    const families = withoutComments.match(/font-family:[^;]*;/g) ?? [];
    expect(families.length).toBeGreaterThan(0);
    for (const declaration of families) {
      if (declaration === MODE_GLYPH_FONT_STACK) continue;
      expect(declaration).toMatch(/^font-family: var\(--nv-font-(prose|mono)\);$/);
    }
  });

  it("the mode-glyph font stack is on .mode-glyph alone, not a grouped or wider selector", () => {
    const rules = withoutComments.match(/[^{}]*\{[^{}]*\}/g) ?? [];
    const owning = rules.filter((rule) => rule.includes(MODE_GLYPH_FONT_STACK));
    expect(owning.length).toBe(1);
    const selector = owning[0].slice(0, owning[0].indexOf("{")).trim();
    expect(selector).toBe(".mode-glyph");
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

  it("admits the surface pair as text only inside a surface-painted box, every branch of it", () => {
    expect(unguardedTextColorDeclarations(".permission-card-stale { color: var(--nv-surface-muted); }")).toEqual([]);
    expect(unguardedTextColorDeclarations(".which-key-box .wk-group { color: var(--nv-surface-fg); }")).toEqual([]);
    expect(unguardedTextColorDeclarations(".session-lost { color: var(--nv-surface-fg); }")).toEqual([
      "color: var(--nv-surface-fg);",
    ]);
    expect(
      unguardedTextColorDeclarations(".session-lost, .permission-card-stale { color: var(--nv-surface-muted); }"),
    ).toEqual(["color: var(--nv-surface-muted);"]);
    // And a card is not a licence for an unguarded colour.
    expect(unguardedTextColorDeclarations(".tool-card { color: var(--nv-warn); }")).toEqual(["color: var(--nv-warn);"]);
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

  it("exempts --nv-syn-keyword as text only on the which-key box's own keycap", () => {
    expect(unguardedTextColorDeclarations(".which-key-box .wk-key { color: var(--nv-syn-keyword); }")).toEqual([]);
    // Anywhere else, and a group with one branch on it, it is refused -- same shape as the hljs-*
    // and chrome-surface proofs above.
    expect(unguardedTextColorDeclarations(".wk-title { color: var(--nv-syn-keyword); }")).toEqual([
      "color: var(--nv-syn-keyword);",
    ]);
    expect(
      unguardedTextColorDeclarations(".session-lost, .which-key-box .wk-key { color: var(--nv-syn-keyword); }"),
    ).toEqual(["color: var(--nv-syn-keyword);"]);
  });

  it("still refuses a different signal colour on the which-key box's own keycap", () => {
    // Proves the narrowed guard did not quietly become "anything inside .wk-key is exempt" -- only
    // --nv-syn-keyword survives it, the same check the hljs-* proof above makes for that exemption.
    const declarations = unguardedTextColorDeclarations(".which-key-box .wk-key { color: var(--nv-warn); }");
    expect(declarations).toEqual(["color: var(--nv-warn);"]);
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

  it("does not dim the activity line or the band with opacity either", () => {
    // Same defect, same fix, same reason: opacity on top of an already-guarded muted text colour
    // would multiply its contrast back down below what tokens.rs actually guaranteed. The winbar
    // is gone (V2, session tabs Task 10); these two replace it and the old status line -- `.status-row`
    // and `.panel-footer`, session tabs Task 10's own replacement, are themselves gone now
    // (panel round 2 plan, Task 10), folded into `.status-band`.
    const rules = withoutComments.match(/\.(?:status-band|activity-line)[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("the status band is one editor row high, and never grows (wave 4, R5)", () => {
    // Owner (issue 3): "agent pane最下面的input >> auto那一行太宽了，最好做到和旁边neovim底下的
    // status一样宽" -- the band used to be `min-height: 24px`, which grows with whatever its
    // children need (the `⏵⏵` fallback symbol font in particular). `--nv-editor-row` is the
    // editor's own cell height (`core/src/theme/tokens.rs`, `ThemeTokens.editor_row_px`); a fixed
    // `height` plus `overflow: hidden` is what actually stops the grow, not just a bigger min.
    const [band] = rulesMatching(withoutComments, ".status-band").filter((r) => r.selector.trim() === ".status-band");
    const decl = (name: string) =>
      band.body
        .split(";")
        .map((d) => d.trim())
        .find((d) => d.startsWith(`${name}:`))
        ?.slice(name.length + 1)
        .trim();
    expect(decl("height")).toBe("var(--nv-editor-row, 24px)");
    expect(decl("min-height")).toBeUndefined();
    expect(decl("box-sizing")).toBe("border-box");
    expect(decl("overflow")).toBe("hidden");
    expect(decl("line-height")).toBe("calc(var(--nv-editor-row, 24px) - 1px)");
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

  /* --- no size query containers (2026-09-24) ---------------------------------------------------
     The owner: "当ai在输入的时候，那个框不能像浏览器一样保持绝对稳定，他会一直改变画面位置到一个固定的布局，
     而且看不到最下面输出". Measured cause (`.superpowers/panel-scroll/root-cause.md`, not in git; the
     dated record's 2026-09-24 (later) entry summarises it): while `.row` was
     `container-type: inline-size`, WebKitGTK 2.52.6 reset `.message-list`'s `scrollTop` to a stale
     value every time the status line's elapsed counter ticked, so a streaming reply stopped being
     followed about a second in. Moving the container to `.row-body`, to `.message-list`, or
     to the assistant rows only was each measured to still break. The real verdict is the WebKitGTK
     test (`shell/tests/panel_stream_scroll.rs`), which needs a display; this is the cheap tripwire
     that runs in `npm test` on a machine without one. It encodes the cause, not the behaviour: a
     query container anywhere in this file has to argue its way past it.

     What it does not see (the fix's review, 2026-09-24): it reads `index.css` only, so a component's
     inline style (`style={{ containerType: … }}`) walks past it; and `contain`/`content-visibility`
     pass by design -- they were never measured on their own, and the rule encodes only the measured
     cause. The WebKitGTK test's S2 (a reader parked inside the streaming reply) is the one scenario
     that still sees a container put back; S1 and S4 no longer do, since the follow effect re-snaps
     before paint. */
  it("declares no size query container anywhere: container-type is normal or absent, and the container shorthand is unused", () => {
    const offending = sizeContainerDeclarations(withoutComments);
    expect(offending, offending.map((d) => `${d.selector} { ${d.declaration} }`).join("\n")).toEqual([]);
    // Positive controls: the scanner does see each shape it forbids -- the exact rule that broke
    // the panel, the shorthand, a case-changed spelling, and one inside `@media` -- and it does not
    // mistake the unrelated `contain` property, or an explicit `normal`, for one.
    const shapes = [
      ".row { container-type: inline-size; }",
      ".row-body { container: body / inline-size; }",
      ".message-list { CONTAINER-TYPE: size; }",
      "@media (min-width: 1px) { .row-assistant { container-type: inline-size; } }",
    ];
    for (const shape of shapes) {
      expect(sizeContainerDeclarations(shape), shape).toHaveLength(1);
    }
    expect(sizeContainerDeclarations(".status-line { contain: strict; } .row { container-type: normal; }")).toEqual([]);
    // And no container-query unit either: with no container to resolve against, a `cqw` silently
    // falls back to the small viewport -- a wrong width with nothing to say so. The escapes spend
    // `var(--row-inline-size)` instead (see index.css's `.row` rule).
    const CONTAINER_UNIT = /\d(?:\.\d+)?cq(?:w|h|i|b|min|max)\b/i;
    expect(withoutComments).not.toMatch(CONTAINER_UNIT);
    expect("width: calc(100cqw - var(--row-gutter));").toMatch(CONTAINER_UNIT);
    expect("min(8 * 1CQI, 120px)").toMatch(CONTAINER_UNIT);
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
    // The duration is four steps of `--meter-step` (300ms unless the typing cadence slows it; the
    // test after this one holds that half), so the resting shorthand is the original 1200ms.
    expect(fill![0]).toMatch(/animation: turn-meter calc\(var\(--meter-step, 300ms\) \* 4\) steps\(4, jump-none\) infinite;/);

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

  it("lets the typing cadence slow the meter through --meter-step, never pause it, and keeps Rust's threshold equal to the meter's step (decision #37)", () => {
    const fill = withoutComments.match(/\.turn-activity \.meter-fill \{[^}]*\}/);
    expect(fill).not.toBeNull();
    const shorthand = fill![0].match(
      /animation: turn-meter calc\(var\(--meter-step, (\d+)ms\) \* (\d+)\) steps\((\d+), jump-none\) infinite;/,
    );
    expect(shorthand).not.toBeNull();
    const [, fallbackMs, factor, steps] = shorthand!;
    // One visible state lasts `--meter-step`: the duration is that times the number of states, so a
    // step of 500ms really is 500ms between repaints, not 125.
    expect(Number(factor)).toBe(Number(steps));
    // Nothing set, the meter costs one repaint per 300ms (1200ms over four states) -- design §3.
    expect(Number(fallbackMs)).toBe(NATURAL_METER_STEP_MS);
    expect(Number(fallbackMs) * Number(steps)).toBe(1200);
    // The same number, in Rust, decides whether the page is told about typing at all
    // (`panel_cadence::SELF_DRIVEN_STEP_MS`): a cadence at least that slow is the only one the meter
    // could outpace. If the CSS step moves and this does not, the page is silently not told.
    const rust = panelCadenceRs.match(/pub const SELF_DRIVEN_STEP_MS: u32 = (\d+);/);
    expect(rust, "core/src/panel_cadence.rs names SELF_DRIVEN_STEP_MS").not.toBeNull();
    expect(Number(rust![1])).toBe(NATURAL_METER_STEP_MS);
    // "Slowed, never paused" (the owner: the stream must stay smooth, not stop): no rule anywhere in
    // the sheet pauses an animation, and the typing attribute has no rule of its own that could.
    expect(withoutComments).not.toMatch(/animation-play-state/i);
    expect(withoutComments).not.toMatch(/data-editor-typing/);
    // Negative control: a pause would be seen by the same check.
    expect(".turn-activity .meter-fill { animation-play-state: paused; }").toMatch(/animation-play-state/i);
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

  /** Defect 4 (phase 2's sandbox pass): a label cut with `…` followed by the working marker read `……`. */
  it("marks a working tab with a separated glyph that is not an ellipsis", () => {
    const rules = splitRules(withoutComments).filter((r) => r.selector.trim() === ".tab-working::after");
    expect(rules).toHaveLength(1);
    expect(rules[0].declarations).toMatch(/content:\s*" ✻"/);
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
  // Panel round 2 (plan Task 10): `.panel-footer .mode-block` is `.band-mode` now, a bare selector
  // (no wrapper prefix -- `StatusBand.tsx` never nests it under a `.panel-footer`, which is gone).
  const MODE_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input">INPUT</span></div>`;
  const UNFOCUSED_INPUT_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input" data-focused="false">INPUT</span></div>`;

  // Review (2026-09-19): the conversation used to be `grid-template-rows: auto 1fr auto auto`, which
  // gives the `1fr` to the SECOND child -- the fatal-error banner when it is shown, not the list.
  // The list then took its full content height and never scrolled, so `j`/`k` could not step
  // through a long reply over a dead session. jsdom has no layout, so this pins the rule that
  // decides it: the list grows by its own class, whatever sits between it and the rest of the
  // panel (V2, session tabs Task 10: the activity line, the composer, the status row, the footer;
  // panel round 2 plan Task 10 replaced the latter two with the one band, formerly the winbar and
  // the status line before that).
  const CONVERSATION = (banner: string) =>
    `<div class="agent-ui-root agent-ui-conversation">${banner}<div class="agent-ui-scroller"><div class="message-list">m</div></div>` +
    `<div class="activity-line">a</div><div class="composer">c</div><div class="status-band">s</div></div>`;

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
      for (const other of [".activity-line", ".composer", ".status-band", ...(banner ? [".fatal-error"] : [])]) {
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
  it("keeps the ? overlay inside the list's own region, not over the bars below it", () => {
    const markup =
      `<div class="agent-ui-root agent-ui-conversation">` +
      `<div class="agent-ui-scroller"><div class="message-list">m</div>` +
      `<div class="keymap-overlay"><section><h2>This panel</h2></section></div></div>` +
      `<div class="activity-line">a</div><div class="status-band">s</div></div>`;
    expect(computed(markup, ".agent-ui-scroller").position).toBe("relative");
    expect(computed(markup, ".keymap-overlay").position).toBe("absolute");
    // And the bars are back to taking part in normal painting: nothing has to out-stack the overlay.
    for (const bar of [".activity-line", ".status-band"]) {
      expect(computed(markup, bar).zIndex).toBe("auto");
    }
  });

  /** R2's pill is the band's own `.band-unread` now (panel round 2 plan, Task 10; spec §5.1: "the
   *  band's right, `↓N` inverted; never over text"), not a `NewPill` floated absolutely over the
   *  list -- the opposite invariant from the one this test used to pin. */
  it("keeps the R2 pill in normal flow inside the band, not floated over the list", () => {
    const markup = `<div class="status-band"><div class="band-right"><button class="band-seg band-unread">↓</button></div></div>`;
    expect(computed(markup, ".band-unread").position).toBe("static");
    expect(computed(markup, ".band-unread").background).toBe("var(--nv-fg)");
    expect(computed(markup, ".band-unread").color).toBe("var(--nv-bg)");
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

  /* --- Task 1 (2026-09-20): wide content escapes the prose measure -----------------------------
     The owner's report: fullscreen leaves a big empty strip down the right of the chat panel,
     because a fenced code block, the permission card's diff and a tool result all wrapped at the
     same measure the `.row` grid caps PROSE at (`index.css:136`'s `minmax(0, var(--prose-measure))`).
     That measure was a hardcoded `62ch` when this task was written; a later change (2026-09-21)
     turned it into the `--prose-measure` token and set it to `1fr` (prose no longer capped at all),
     but the escape mechanism these tests guard -- content that ignores whatever the measure is --
     is unchanged, so the tests keep their name and their point; only the literal "62ch" wording
     that named the measure by its old value is gone.

     jsdom runs no real Grid layout (see the big comment atop this describe block), so none of
     these tests can see an actual resolved pixel width -- what they CAN see, and what the fix
     actually is, is a `cqw`-based `width` declaration reaching a specific element and not others.
     A `cqw` unit is relative to `.row`'s own `container-type: inline-size` (also new), which is
     what lets it reach `.tool-result-body`/`.permission-card-edit` through several PLAIN wrapper
     elements (`.tool-call`, `[data-awaiting-permission]`, `.permission-card`) that a subgrid
     alternative could not reach without giving up their own box (background/border/padding).

     Correction (2026-09-24): the mechanism is `var(--row-inline-size)` now, not `cqw`. The query
     container these tests relied on made WebKitGTK reset the list's scroll position while a reply
     streamed (see the "declares no size query container" tripwire above and index.css's `.row`
     rule), so it is gone; `.row` derives `--row-inline-size` from the list's measured width, and a
     custom property reaches through the same plain wrappers a container query did. What each test
     below pins is unchanged: a width that spends the ROW's width reaching exactly these elements and
     not others. `evaluatePx` (top of file) turns the declarations into numbers, so the right-edge
     claim is checked as arithmetic rather than as a spelling. */
  it("lets a fenced code block ignore the prose measure, unlike a paragraph in the very same row", () => {
    // Real defect reproduced: before this task, `.code-block` had no `width` rule at all, so it
    // was exactly as capped by `.row-body`'s grid track as the paragraph beside it.
    const html =
      `<div class="message-list"><div class="row row-assistant"><div class="row-body">` +
      `<p>prose</p><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const prose = computed(html, "p");
    const code = computed(html, "pre.code-block");
    expect(code.width).toMatch(/var\(--row-inline-size\)/);
    expect(prose.width).not.toMatch(/--row-inline-size/);
    // Negative control: without the rule this task adds, a code block is exactly as capped as the
    // paragraph next to it -- there is nothing else in this file that would widen it.
    expect(computed(html, "pre.code-block", ".code-block { width: auto; }").width).not.toMatch(/--row-inline-size/);
  });

  it("lets a tool result ignore the prose measure through two plain, unstyled wrapper elements", () => {
    // `.tool-result-body` sits inside `[data-awaiting-permission]` > `.tool-call` >
    // `.tool-result` -- MessageList.tsx and toolRegistry.tsx's real nesting -- and NONE of those
    // three carries a rule in this file. A fix that (wrongly) targeted `.row-body` itself, rather
    // than the leaf, would not be exercised by a fixture this shallow; this one is exactly as deep
    // as the real DOM to make sure the row's width really does reach through, not just past a
    // fixture shortcut.
    const html =
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div data-awaiting-permission><div class="tool-call"><div class="tool-result">` +
      `<pre class="tool-result-body">out</pre></div></div></div></div></div></div>`;
    expect(computed(html, "pre.tool-result-body").width).toMatch(/var\(--row-inline-size\)/);
    expect(computed(html, "pre.tool-result-body", ".tool-result-body { width: auto; }").width).not.toMatch(
      /--row-inline-size/,
    );
  });

  it("lays a card's command and input out in the order of their characters", () => {
    // Direction controls on the card are escaped; what remains is the implicit reordering of
    // right-to-left letters, which this rule turns off so the line reads in byte order.
    for (const selector of [".permission-card-command", ".permission-card-input", ".permission-card-edit-path", ".permission-card-diff"]) {
      const cls = selector.slice(1);
      const style = computed(`<pre class="${cls}">x</pre>`, selector);
      expect(style.getPropertyValue("unicode-bidi"), selector).toBe("bidi-override");
      expect(style.getPropertyValue("direction"), selector).toBe("ltr");
    }
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
    expect(edit.width).toMatch(/var\(--row-inline-size\)/);
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
      /--row-inline-size/,
    );
  });

  /* Whole-branch review (2026-09-20): the raw-JSON `<pre>` is the FIFTH element under the prose
     measure and the one the product draws most, since `editPreview` returns null for
     `Bash`/`WebFetch`/an unknown tool -- which per CLAUDE.md's permission-policy row is nearly
     every card. It is the same class of miss the review already caught once for
     `.tool-card-generic`. (The measure itself was `62ch` when this comment was written; see the
     Task 1 header above for why the name outlived the number.) */
  it("lets a permission card's raw-JSON input ignore the prose measure, with the card's own inset backed out", () => {
    const html =
      `<div class="message-list"><div class="row row-permission"><div class="row-body">` +
      `<div class="permission-card"><pre class="permission-card-input">{"command":"..."}</pre></div>` +
      `</div></div></div>`;
    const input = computed(html, ".permission-card-input");
    expect(input.width).toMatch(/var\(--row-inline-size\)/);
    // The same -11px as the diff box above, and for the same reason: both sit inside
    // `.permission-card`'s border + padding, so both have to climb back out by the same amount or
    // the two would not line up with each other, let alone with a code block.
    expect(input.marginLeft).toBe("-11px");
    expect(input.marginLeft).toBe(computed(html, ".permission-card-input").marginLeft);
    // Negative control: without this rule it is exactly as capped as it was.
    expect(computed(html, ".permission-card-input", ".permission-card-input { width: auto; margin-left: 0; }").width).not.toMatch(
      /--row-inline-size/,
    );
  });

  /* Whole-branch review (2026-09-20): `calc(100cqw - 30px)` assumes `.tool-result-body`'s parent is
     flush with `.row-body`. On the FAILED-tool path it is not -- `.tool-result-error` puts a 2px
     border and 6px of padding on that exact parent -- so the body overhung `.row`'s right edge by
     8px. The original fixture used a plain `.tool-result`, so neither the prose nor the suite
     covered the one state where the rule was untrue.

     Correction (2026-09-21): the literal `30px`/`38px` this test used to pin are gone -- the escape
     now subtracts `var(--row-gutter)`, and jsdom does not resolve `var()` in `getComputedStyle` at
     all (confirmed directly: the computed `width` comes back as the literal author text
     `"calc(100cqw - var(--row-gutter))"`, not normalised into any particular order), so pinning the
     unresolved string would only prove the file still spells the token's name, not that the
     arithmetic is right. `expandVars` (top of file) resolves `var(--row-gutter)` against the real
     `:root` declaration instead, so this still checks the actual subtraction -- now visibly built
     from the sign column's own two tokens rather than from a number nothing derives.

     Correction (2026-09-24): `100cqw` is `var(--row-inline-size)` now (no query container; see the
     tripwire near the top of the `index.css` describe block), which `expandRowVars` resolves through
     `.row`'s own declaration, and `evaluatePx` checks the result as numbers. */
  it("narrows a tool result by its error gutter, so a FAILED tool lands on the same right edge as a passing one", () => {
    const row = (errorClass: string) =>
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div class="tool-call"><div class="tool-result${errorClass}">` +
      `<pre class="tool-result-body">out</pre></div></div></div></div></div>`;
    // `--row-gutter` is `calc(var(--row-sign-w) + var(--row-gap))` = `calc(22px + 8px)`, and
    // `--row-inline-size` is `.row`'s own `calc(var(--list-inline-size, 0px) - 2px)`, so the base
    // escape expands to exactly the row's width minus the gutter, inside the "no escape" floor.
    const base = computed(row(""), "pre.tool-result-body").width;
    const failed = computed(row(" tool-result-error"), "pre.tool-result-body").width;
    expect(expandRowVars(base)).toBe("max(100%, calc(calc(var(--list-inline-size, 0px) - 2px) - calc(22px + 8px)))");
    // 2px border-left + 6px padding-left = 8px, so the error state subtracts the gutter AND that
    // 8px, and the two right edges coincide.
    expect(expandRowVars(failed)).toBe(
      "max(100%, calc(calc(var(--list-inline-size, 0px) - 2px) - calc(22px + 8px) - 8px))",
    );
    // As numbers, for a 600px list whose body track is capped narrower (the `62ch` opt-in): the
    // passing body is the row's 598px minus the 30px gutter, the failed one 8px less, and each ends
    // on the row's right edge because each starts 8px apart. With no measurement yet, both fall back
    // to their containing block (`100%`), never to a negative width.
    expect(evaluatePx(base, 600, 400)).toBe(568);
    expect(evaluatePx(failed, 600, 392)).toBe(560);
    expect(evaluatePx(base, null, 400)).toBe(400);
    expect(evaluatePx(failed, null, 392)).toBe(392);
    // Negative control: this is a real specificity win, not an accident of the fixture. Deleting
    // the narrower rule (simulated by re-declaring the base one after it, at higher specificity)
    // puts the overhanging (gutter-only) width back.
    expect(
      expandRowVars(
        computed(
          row(" tool-result-error"),
          "pre.tool-result-body",
          ".tool-result-error .tool-result-body { width: max(100%, calc(var(--row-inline-size) - var(--row-gutter))); }",
        ).width,
      ),
    ).toBe("max(100%, calc(calc(var(--list-inline-size, 0px) - 2px) - calc(22px + 8px)))");
  });

  /* --- the in-flight indicator's own layout (whole-branch review, 2026-09-20) ------------------
     Two findings, one bar. jsdom runs no flex layout, so both tests read the DECLARATIONS that
     decide the outcome -- which is the same thing every other test in this block does, and is
     exactly why the defects were invisible until someone did the flexbox arithmetic by hand. */

  it("keeps the meter from being the first thing squeezed off a narrow activity line", () => {
    // `.meter`'s only child is an EMPTY span, so its min-content size is 0 and flexbox's automatic
    // minimum (§4.5) is min(4ch, 0) = 0. As an ordinary flex item it shrank to nothing -- the one
    // animated element in the product silently gone on the narrow panel where it matters most.
    const html = `<div class="activity-line"><span class="turn-activity" data-phase="thinking"><span class="meter"><span class="meter-fill"></span></span><span class="turn-state">thinking</span></span></div>`;
    const meter = computed(html, ".meter");
    expect(meter.flexShrink).toBe("0");
    expect(meter.flexGrow).toBe("0");
    // `4ch` used to be pinned through this engine's own resolution of it -- `ch` at 0.5em against
    // `.activity-line`'s literal `font-size: 12px`, so 24px. **That stopped being testable here on
    // 2026-09-21**, when every size in this file became a ratio of `--nv-font-size`: jsdom does not
    // resolve `var()` AT ALL (the grid-track assertions elsewhere in this file read back
    // `var(--row-sign-w) ...` as text, which is the same fact), so `.activity-line`'s font-size is
    // unresolvable, `ch` falls back to the inherited 16px, and the number here became 32 -- a
    // measurement of jsdom's fallback, not of this stylesheet.
    //
    // So it is split in two, and neither half is the old assertion weakened. What jsdom CAN see is
    // that the declaration really says `4ch`; what decides the real pixels is the scale, and that
    // is pinned directly by "the text scale is one number and five ratios of it" below, which
    // evaluates `--fs-sm` to 12px at the shipped base. Between them the old number is still
    // guarded, and neither half passes if the other's half of the mechanism breaks.
    expect(winningDeclaration(html, ".meter", "width")).toBe("4ch");
    // The same idiom, on the element this file already used it on -- so the fix is the file's own
    // answer rather than a new one.
    const gutter = computed(`<div class="diff-line"><span class="diff-gutter">+</span></div>`, ".diff-gutter");
    expect(gutter.flexShrink).toBe("0");
    // Negative control: the default `flex-shrink: 1` put back is the defect, and it is reachable
    // from any later rule that touches `flex` on this element.
    expect(computed(html, ".meter", ".turn-activity .meter { flex: 1 1 auto; }").flexShrink).toBe("1");
  });

  /** Round-3 review: the scale's arithmetic test reads `ROOT_TOKENS`, which is built by the
   *  `:root`-only regex the container-unit guard was deliberately moved OFF. Redefining `--fs-sm`,
   *  `--fs-xs` and `--fs-code` to literals inside
   *  `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { ... } }` pinned three
   *  sizes against `agent.font_size` and passed green -- and `data-theme` is set nowhere in
   *  `agent-ui/web/src`, so that `:not()` always matches.
   *
   *  Rather than make `ROOT_TOKENS` see everywhere (it is right as it is: `expandVars` wants the
   *  ONE scope a browser resolves this file's own `var()`s against), this pins the premise that
   *  makes `ROOT_TOKENS` trustworthy -- there is nowhere else for these tokens to be declared. It
   *  scans `allCustomPropertyDeclarations`, the brace-aware walker that DOES see everywhere. */
  it("declares every panel token exactly once, on a plain :root, with nowhere else for one to hide", () => {
    const PANEL_TOKEN = /^--(row|prose|prompt|fs)-/;
    const declared = allCustomPropertyDeclarations(withoutComments).filter((d) => PANEL_TOKEN.test(d.name));
    // A premise, not a formality: if the walker stopped finding them this test would pass empty.
    expect(declared.length, "the panel's own tokens are not being found at all").toBeGreaterThanOrEqual(11);

    const sites = new Map<string, string[]>();
    for (const d of declared) sites.set(d.name, [...(sites.get(d.name) ?? []), d.selector.trim()]);
    // The one exception, by name (2026-09-24): `--row-inline-size` is not a knob but the row's own
    // width, derived from the list's measured one, so it can only be declared where it resolves --
    // on `.row` (a declaration on `:root` would resolve its `var(--list-inline-size)` there, where
    // nothing sets it). Pinned to exactly that one site, visible to `ROW_TOKENS`, so this is not a
    // door for a second place to declare anything else.
    const DECLARED_ON_ROW = new Set(["--row-inline-size"]);
    for (const name of DECLARED_ON_ROW) {
      expect(sites.get(name), `${name} must be declared exactly once, on .row`).toEqual([".row"]);
      expect(ROW_TOKENS.has(name), `${name} is declared but invisible to ROW_TOKENS`).toBe(true);
    }
    for (const [name, selectors] of sites) {
      if (DECLARED_ON_ROW.has(name)) continue;
      expect(
        selectors,
        `${name} is declared ${selectors.length} times (${selectors.join(" | ")}) -- a second declaration ` +
          `anywhere, including inside an @media, silently overrides the one every guard here reads`,
      ).toHaveLength(1);
      expect(
        selectors[0],
        `${name} is declared on "${selectors[0]}", not on a plain :root, so ROOT_TOKENS cannot see it`,
      ).toBe(":root");
      expect(ROOT_TOKENS.has(name), `${name} is declared but invisible to ROOT_TOKENS`).toBe(true);
    }
    // ...and `.row` declares nothing else a guard here would have to know about.
    expect([...ROW_TOKENS.keys()]).toEqual([...DECLARED_ON_ROW]);
  });

  /* --- the text scale is one number and five ratios of it (2026-09-21) -------------------------
     `--nv-font-size` replaced thirty-one hand-written `font-size` literals at six values with one
     Rust-emitted base and five `--fs-*` tokens, each a ratio of it (index.css's own comment above
     the `:root { --fs-xs: ...; }` block). These four guard that the change actually landed as a
     SYSTEM, not just as a fresh set of numbers that happen to render the same as the old ones:
     every `font-size` in the file spends a token rather than a literal, each token's ratio computes
     to the exact pixel value the file used to hardcode, the one literal that remains (the `14px`
     fallback) is not a second, driftable copy of Rust's own default, and the name carrying that
     fallback is one `tokens.rs` actually emits. */

  it("spends every font-size as a var(--fs-*) ratio, never a literal length", () => {
    // Round-2 review, item 8: a whole scale is worth nothing if one call site still hardcodes a
    // length -- that one rule would not move when the owner changes `agent.font_size`, and nothing
    // above (which checks the TOKENS, not their USE) would notice. `.toMatch` on every declaration
    // is the whole guard; the count is a positive control so a mass-deletion of `font-size:` lines
    // cannot make the loop below pass by running zero times.
    const declarations = withoutComments.match(/font-size:[^;]*;/g) ?? [];
    expect(declarations.length).toBeGreaterThanOrEqual(31);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^font-size: var\(--fs-[a-z]+\);$/);
    }
  });

  it("computes the six --fs-* tokens as exact ratios of one base, by arithmetic rather than by matching the fraction text", () => {
    // The six values this file used to hardcode, and the ratio each token's own comment says it
    // replaced -- evaluated, not string-matched (see `evaluateFsExpression`'s own doc for why a text
    // match on `"* 12 / 14"` cannot tell a correct ratio from a wrong one).
    const expected: [string, number][] = [
      ["--fs-xs", 11],
      ["--fs-sm", 12],
      ["--fs-code", 12.5],
      ["--fs-md", 13],
      ["--fs-base", 14],
      ["--fs-prose", 15],
    ];
    for (const [token, px] of expected) {
      const raw = ROOT_TOKENS.get(token);
      expect(raw, `${token} is not declared`).toBeDefined();
      expect(evaluateFsExpression(raw!)).toBeCloseTo(px, 10);
    }
    // Negative control: the exact regression this guards -- a digit transposed in the ratio (`13 /
    // 14` written where `12 / 14` belongs) is caught by the WRONG NUMBER it evaluates to, not by a
    // string comparison that a differently-spelled but equally wrong fraction could still slip past.
    expect(evaluateFsExpression("calc(var(--nv-font-size, 14px) * 13 / 14)")).not.toBeCloseTo(12, 10);

    // The meter test above splits `4ch @ .activity-line` into "the declaration says 4ch" (jsdom can
    // see that) and "the scale says .activity-line's font-size is 12px" (only arithmetic can say
    // that) because jsdom resolves neither `var()` nor the activity line's real font-size at all.
    // This is where the two halves actually meet: `.activity-line` is `font-size: var(--fs-sm)`,
    // `ch` is 0.5em, and 4 * 0.5 * 12 is the 24px the pre-2026-09-21 file hardcoded directly. If
    // either half drifts -- the token renamed off `.activity-line`, or `--fs-sm`'s ratio changed --
    // this number moves and says so; today it still lands on the number the meter test's own
    // comment names.
    expect(4 * 0.5 * evaluateFsExpression(ROOT_TOKENS.get("--fs-sm")!)).toBe(24);
  });

  it("keeps the 14px fallback equal to Rust's own DEFAULT_PANEL_FONT_SIZE_PX, not a second hand-copied number", () => {
    // index.css's own comment states this equality by hand ("The literal is a second copy of Rust's
    // `DEFAULT_PANEL_FONT_SIZE_PX`, and `indexCss.test.ts` holds the two equal") -- this is that
    // test. Two independently hand-copied numbers agreeing today is exactly the shape of defect this
    // whole change removed elsewhere in the file (the old `30px`/`38px` pair `--row-gutter` replaced,
    // named in this file's own "panel-width geometry" block above); reusing the same `tokensRs` raw
    // text every other guard in this file already parses, rather than writing `14` down a second
    // time in this test, is what keeps this guard from becoming the third hand-copied instance of
    // its own number.
    const rustConstant = tokensRs.match(/pub const DEFAULT_PANEL_FONT_SIZE_PX:\s*f32\s*=\s*([\d.]+);/);
    expect(rustConstant, "DEFAULT_PANEL_FONT_SIZE_PX not found in tokens.rs -- the parse is broken, not the CSS").not.toBeNull();
    const cssFallback = ROOT_TOKENS.get("--fs-base")!.match(/^var\(--nv-font-size,\s*([\d.]+)px\)$/);
    expect(cssFallback, "--fs-base's fallback is not the plain var(--nv-font-size, <n>px) shape this test expects").not.toBeNull();
    expect(parseFloat(cssFallback![1])).toBe(parseFloat(rustConstant![1]));
    // Red-check performed by hand for this round (not left as a live assertion, since it would mean
    // editing the real file): changing either side alone -- `14px` in index.css's five `calc()`s and
    // one `var()`, or `14.0` in tokens.rs -- while leaving the other, makes this line fail; that was
    // confirmed and reverted rather than committed.
  });

  it("reads --nv-font-size's fallback spelling, not just its bare form, as a name Rust emits", () => {
    // Round-2 review, item 11: `var(--nv-font-size, 14px)` is a NEW spelling in this file -- every
    // other `var(--nv-*)` use up to 2026-09-21 was the bare, no-fallback form the `--nv-* names`
    // describe block's own regex (`/var\((--nv-[a-z-]+)/g`) was written against. Confirmed directly,
    // not assumed: the capturing group `[a-z-]+` stops at the first character outside that class,
    // which the comma before the fallback value already is, so the regex captures exactly
    // `--nv-font-size` and no more -- this pins that down as a passing property of the real regex
    // rather than leaving it to be re-discovered (or to silently start failing) the next time
    // someone adds a `var(--nv-*, ...)` with a fallback.
    const used = Array.from(withoutComments.matchAll(/var\((--nv-[a-z-]+)/g), (m) => m[1]);
    expect(used).toContain("--nv-font-size");
    // And the isolated case, so a future change to the SHARED regex that breaks this is caught here
    // even if `--nv-font-size` itself were ever removed from the real file.
    expect(Array.from("var(--nv-font-size, 14px)".matchAll(/var\((--nv-[a-z-]+)/g), (m) => m[1])).toEqual([
      "--nv-font-size",
    ]);
  });

  it("spends the phase word before the Stop button as the activity line narrows", () => {
    // The priority order index.css states in prose: `.turn-activity` (the indicator's whole box) is
    // the only child of `.activity-line` that gives, and inside that box the phase word is the only
    // thing that can give. The meter, the clock and Stop are never spent. The Stop button is the one
    // that matters -- App.tsx's own comment says it is the ONLY Stop control for mouse users, so it
    // going off the right edge is the interrupt affordance leaving the screen.
    //
    // **V2 (session tabs Task 10) narrowed this from a three-way distribution to this one.** The
    // mode block, the session status word and the position counter left `.status-line` entirely (the
    // status word is not shown in V2 at all; the position moved into `StatusRow`'s own text) -- so
    // there is no longer a second thing in the row that could compete with the indicator for space,
    // and the two-decision structure the 2026-09-20 re-review pinned collapses into one.
    const html =
      `<div class="activity-line">` +
      `<span class="turn-activity" data-phase="tool"><span class="meter"><span class="meter-fill"></span></span>` +
      `<span class="turn-state">running NotebookEdit</span><span class="turn-elapsed">123s+</span></span>` +
      `<button type="button" class="stop">Stop</button></div>`;
    const shrink = (selector: string) => Number(computed(html, selector).flexShrink);

    const dom = new DOMParser().parseFromString(html, "text/html");
    expect(dom.querySelector(".turn-state")?.parentElement?.className).toBe("turn-activity");

    // `.turn-activity` is the only child of `.activity-line` that can give; Stop never does.
    expect(shrink(".turn-activity")).toBeGreaterThan(0);
    expect(shrink(".stop")).toBe(0);

    // Inside `.turn-activity`: the phase word is the ONLY item there that can give, so
    // `.turn-activity` shrinking IS the phase word truncating.
    expect(shrink(".meter")).toBe(0);
    expect(shrink(".turn-elapsed")).toBe(0);
    expect(shrink(".turn-state")).toBeGreaterThan(0);

    // ...and what `flex: none` on the meter and the clock actually buys: the DISTRIBUTION never
    // spends them. It does not mean they can never be clipped -- once the word is gone there is
    // nothing left inside the box to give -- so `.turn-activity` clips its own overflow rather
    // than letting the meter and the clock run out over Stop.
    expect(computed(html, ".turn-activity").overflow).toBe("hidden");

    // A word that shrinks has to be ABLE to: its automatic minimum is its min-content size (the
    // whole word) unless `min-width: 0` says otherwise, and `text-overflow` never engages without
    // an `overflow` that is not `visible`.
    expect(computed(html, ".turn-state").minWidth).toBe("0px");
    expect(computed(html, ".turn-state").overflow).toBe("hidden");
    expect(computed(html, ".turn-state").textOverflow).toBe("ellipsis");
    // `.turn-activity` is itself a flex ITEM: its child cannot shrink unless it can.
    expect(computed(html, ".turn-activity").minWidth).toBe("0px");
    // And the bar never becomes two lines.
    expect(computed(html, ".activity-line").flexWrap).not.toBe("wrap");
    expect(computed(html, ".activity-line").whiteSpace).toBe("nowrap");
    // Negative control: the state this replaced -- nothing shrinking, nothing clipping -- is what
    // pushed Stop off the edge, and it is one later `flex` declaration away.
    expect(computed(html, ".turn-state", ".activity-line .turn-activity .turn-state { flex: none; }").flexShrink).toBe("0");
  });

  /* Review of Task 1: an unrecognized tool's raw JSON dump is a fourth `<pre>` under the same
     prose-measure-capped `.row-body` and had been missed the first time this list was written.
     Widens the SAME element the fenced-code-block test above does (the outer box, not the bare
     `<pre>` inside it) -- `.tool-card-generic` needs no compensating `margin-left`, unlike
     `.permission-card-edit` above, because nothing between it and `.row-body` (`.tool-call`,
     `[data-awaiting-permission]`) carries a rule in this file either. */
  it("lets an unrecognized tool's raw JSON dump ignore the prose measure too", () => {
    const html =
      `<div class="message-list"><div class="row row-tool"><div class="row-body">` +
      `<div data-awaiting-permission><div class="tool-call">` +
      `<details class="tool-card tool-card-generic"><summary>s</summary><pre>{}</pre></details>` +
      `</div></div></div></div></div>`;
    const card = computed(html, ".tool-card-generic");
    expect(card.width).toMatch(/var\(--row-inline-size\)/);
    expect(card.marginLeft).not.toBe("-10px");
    // Negative control: without the rule this fix adds, it is exactly as capped as everything else
    // in `.row-body`.
    expect(computed(html, ".tool-card-generic", ".tool-card-generic { width: auto; }").width).not.toMatch(/--row-inline-size/);
  });

  it("keeps the sign column on the same track whether the row's body is prose or a wide code block", () => {
    // The entire reason the grid exists (`index.css:136-150`'s comment): the sign column must sit
    // at the same x position down the whole page. Widening the BODY column must never be able to
    // move it, so this compares `.row`'s own `grid-template-columns` -- the declaration that
    // decides column 1's width -- across a plain-prose row and a row whose body is a wide code
    // block. jsdom does not resolve `var()`, so both sides come back as the literal token text
    // (`"var(--row-sign-w) minmax(0, var(--prose-measure))"`) regardless of which fixture produced
    // them -- `expandVars` (top of file) resolves that against the real `:root` declarations so the
    // pin is still on the real, current value (`--prose-measure: 1fr`) and not on the unresolved
    // string, which would pass even if `.row`'s own selector stopped applying at all.
    const proseRow =
      `<div class="message-list"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><p>prose</p></div></div></div>`;
    const codeRow =
      `<div class="message-list"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const proseColumns = expandVars(computed(proseRow, ".row").gridTemplateColumns);
    expect(proseColumns).toBe("22px minmax(0, 1fr)");
    expect(expandVars(computed(codeRow, ".row").gridTemplateColumns)).toBe(proseColumns);
    // Negative control: proves the assertion above can actually fail. A rule that changed column 1
    // wins the SAME assertion path used above, at the same specificity as `.row`'s own base rule
    // (`.row-assistant`, 0,1,0), placed after it -- reproducing the shape a careless edit to this
    // grid, or to a class every row already carries, would take. The override is plain literals (no
    // `var()`), so `expandVars` leaves it untouched -- it still has to differ from `proseColumns`.
    expect(
      expandVars(
        computed(codeRow, ".row", ".row-assistant { grid-template-columns: 24px minmax(0, 62ch); }").gridTemplateColumns,
      ),
    ).not.toBe(proseColumns);
  });

  it("derives `.row`'s own width from the list's measured one, with no size container and its grid tracks untouched", () => {
    // The mechanism the escape tests above all rely on, pinned on its own so a regression here
    // explains itself instead of surfacing as several unrelated-looking failures above. Until
    // 2026-09-24 this test asserted the opposite -- `container-type: inline-size` -- and that
    // container is what made WebKitGTK drop the reader while a reply streamed.
    const row = computed(CODE_BLOCK, ".row");
    expect(row.getPropertyValue("container-type")).toBe("normal");
    // `--row-inline-size` is the list's content-box width minus THIS rule's own 2px `border-left`
    // (taken out of the content box by the global `box-sizing: border-box`) -- which is what `100cqw`
    // measured. The 2px is read back from the border, not trusted from the formula.
    expect(ROW_TOKENS.get("--row-inline-size")).toBe("calc(var(--list-inline-size, 0px) - 2px)");
    expect(row.borderLeftWidth).toBe("2px");
    expect(row.boxSizing).toBe("border-box");
    expect(row.display).toBe("grid");
    expect(expandVars(row.gridTemplateColumns)).toBe("22px minmax(0, 1fr)");
    // Negative control: the container put back is exactly what this and the tripwire refuse.
    expect(computed(CODE_BLOCK, ".row", ".row { container-type: inline-size; }").getPropertyValue("container-type")).toBe(
      "inline-size",
    );
  });

  it("leaves a choice row on the sign-column grid, not on the boxed-button padding", () => {
    const row = computed(CHOICE_ROW, "button");
    expect(row.display).toBe("grid");
    expect(expandVars(row.gridTemplateColumns)).toBe("22px minmax(0, 1fr)");
    // `.mode-selector button:not(.row-choice)`'s 10px would show up here; `.row`'s own is `2px 0`
    // and `.row-choice` narrows it to `4px 0`, so the left padding is the tell.
    expect(row.paddingLeft).toBe("0px");
    const clobbered = computed(CHOICE_ROW, "button", ".mode-selector button { display: block; padding: 10px; }");
    expect(clobbered.display).toBe("block");
    expect(clobbered.paddingLeft).toBe("10px");
  });

  /* --- the panel-width geometry, as tokens, is actually derived (2026-09-21) -------------------
     `--row-gutter` replaced two independent magic numbers (`.row`'s `22px`/`8px` and every escape
     rule's own `30px`) that used to have to agree by hand, and nothing checked that they did. These
     four guard the parts of that fix a passing width-string assertion above cannot: that the
     formula really is a formula, that no rule anywhere still spells the sign column's width as its
     own number, and that the WebKitGTK-specific reason `--row-gutter` carries no unit at all is a
     property of every custom property in the file, not just this one. */

  it("derives --row-gutter from the sign column instead of copying its sum, and every escape spends it", () => {
    // Half 1: the token's own declared value has to be a formula referencing the two knobs that
    // make up the sign column, not a copy of their sum -- `calc(30px)` would satisfy every other
    // test in this file just as well as `calc(var(--row-sign-w) + var(--row-gap))` does.
    const gutter = ROOT_TOKENS.get("--row-gutter");
    expect(gutter).toBeDefined();
    expect(gutter).toMatch(/var\(--row-sign-w\)/);
    expect(gutter).toMatch(/var\(--row-gap\)/);
    // Half 2: a correct formula sitting unused proves nothing -- every escape in the real file (a
    // `width` spending `var(--row-inline-size)`; `calc(100cqw ...)` until 2026-09-24) has to actually
    // spend it, or a rule could still subtract its own literal right beside a perfectly good token.
    // Four: the code block / tool result / generic card group, the failed tool result, and the
    // permission card's input and diff box.
    const escapes = rowEscapeWidths(withoutComments);
    expect(escapes.length).toBe(4);
    for (const escape of escapes) {
      expect(escape, escape).toMatch(/var\(--row-gutter\)/);
      // ...and each one degrades to its own containing block before the list has been measured,
      // never below it (`max(100%, ...)`; see index.css's `.row` rule).
      expect(escape, escape).toMatch(/^max\(100%,/);
    }
  });

  it("keeps a container-query unit out of every custom property declaration (WebKitGTK guard)", () => {
    // Since 2026-09-24 there is no container-query unit anywhere in the file (the tripwire near the
    // top of the `index.css` block: with no container, a `cq*` unit would silently resolve against
    // the viewport). This guard is kept for what it pins about custom properties specifically, and
    // because its bypasses are still real ones. What follows is its original rationale.
    //
    // Spelled out in index.css's own comment: a `var()` substitutes as raw tokens, so a `cqw` unit
    // written INSIDE a custom property's value would still resolve correctly by spec -- but
    // container units inside custom properties are exactly the corner where an engine has shipped
    // bugs, this panel runs in WebKitGTK, and the failure mode is not a wrong number in one place,
    // it is the whole panel resolving against the wrong axis. So every escape rule spells its own
    // `100cqw` and no `--custom-property`'s declared value may contain a `cq*` unit at all --
    // wherever in the file it is declared, not merely inside an unqualified top-level `:root`.
    //
    // Scans `allCustomPropertyDeclarations`, not `ROOT_TOKENS`: `ROOT_TOKENS` is built from a regex
    // anchored on the literal text `:root\s*\{`, which cannot see a declaration inside e.g.
    // `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --foo: 10cqw; } }`
    // -- there is a `:not(...)` between `:root` and `{`, so that whole block is invisible to the
    // regex and a container-query unit smuggled in there passed this guard green. Reproduced and
    // confirmed red against the real file below.
    const declarations = allCustomPropertyDeclarations(withoutComments);
    expect(declarations.length).toBeGreaterThan(0);
    for (const { name, value, selector } of declarations) {
      expect(value, `${selector} declares ${name} with a container-query unit: ${value}`).not.toMatch(
        /cq(w|h|i|b|min|max)/i,
      );
    }
    // Negative control, the exact shape of the reviewer's bypass (adapted to a real token name --
    // `--prompt-inset` itself no longer exists, but the escape mechanism is unchanged): nested
    // inside `@media`, and reached through `:root:not(...)` rather than a bare `:root`.
    const bypass =
      '@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --row-sign-w: 10cqw; } }';
    expect(allCustomPropertyDeclarations(bypass)).toEqual([
      { name: "--row-sign-w", value: "10cqw", selector: ":root:not([data-theme=\"light\"])" },
    ]);
    // ...and the OLD mechanism really did miss it -- so this is a reproduced bypass, not a test
    // invented against a strawman.
    expect(rootCustomProperties(bypass).size).toBe(0);

    // Round-2 review, a second bypass of the SAME guard: a multi-line VALUE rather than an unusual
    // SELECTOR. `--bypass` here is a real container-query escape (a bare `100cqw`, exactly what this
    // guard exists to forbid), split across three lines the way `index.css`'s own `margin-left:
    // clamp(...)` already is -- the `s` flag fix above is what makes this line find it at all.
    const multiline = ":root {\n  --bypass: calc(\n    100cqw\n  );\n}";
    const multilineDecls = allCustomPropertyDeclarations(multiline);
    expect(multilineDecls).toEqual([{ name: "--bypass", value: "calc(\n    100cqw\n  )", selector: ":root" }]);
    // The value this finds really would fail the guard's own assertion above -- the fix makes the
    // scan SEE the declaration, not exempt it.
    expect(multilineDecls[0].value).toMatch(/cq(w|h|i|b|min|max)/i);
    // ...and the pre-fix regex (no `s` flag) really did drop it -- reproduced here rather than
    // asserted from the comment alone, using the same expression this function carried before the
    // round-2 review with only the flag removed.
    const preFixRegex = /^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/;
    expect("\n  --bypass: calc(\n    100cqw\n  )\n".match(preFixRegex)).toBeNull();
  });

  it("never lets a literal number sneak back into a row-width escape", () => {
    // (Until 2026-09-24 the escapes were `calc(100cqw - ...)`; they spend `var(--row-inline-size)`
    // now, and this scan follows them.) The regression guard for the derivation above, written as
    // its own direct string scan (not a reuse of `rowEscapeWidths`' declaration split) so a defect
    // in that helper cannot hide a regression from both checks at once. Matches only a NUMBER immediately after the `-`, so it
    // does not fire on `.tool-result-error`'s legitimate extra `- 8px` (that one is its own gutter,
    // not a second copy of the sign column, and is covered by the test above instead).
    //
    // Positive control (round-2 review): this test used to rely entirely on a SIBLING test elsewhere
    // in this file to prove `100cqw` still exists in the real stylesheet at all, which means IT ALONE
    // passes vacuously against a zero-byte (or renamed-away) stylesheet -- the negative assertion
    // below is true of the empty string for free. Made self-sufficient rather than left dependent on
    // another `it` block staying green for an unrelated reason.
    expect(withoutComments).toMatch(/var\(--row-inline-size\)/);
    expect(withoutComments).not.toMatch(/var\(--row-inline-size\)\s*-\s*\d/);
  });

  /* --- the user's own message is an indent, not a bubble (2026-09-21) --------------------------
     `.row-prompt .row-body`'s four new declarations (displacement, rule, colour) are a design
     decision stated in index.css's own comment: take Cursor's idea (distinguish the two speakers by
     position) without taking Cursor's look (a filled, rounded, right-hugging pill). These four
     guard that the decision actually landed and that nothing already in this file, or added later,
     quietly turns the indent back into a card. */

  it("carries the prompt row's displacement, rule and colour on .row-prompt .row-body, and the cascade lets each win", () => {
    // `border-left: 2px solid var(--nv-fg)` is a shorthand containing a `var()`, which this
    // jsdom engine drops from computed style entirely (see the big comment atop this describe
    // block, and the `--nv-border` test near the top of the file for the same idiom) -- so its
    // TEXT is checked here and its CASCADE is checked separately below through `winningDeclaration`.
    //
    // Wave 3 Task 2 (variant C): the rule moved from `--nv-muted` to `--nv-fg`, the owner's
    // "现在的ui不错" starting point pushed one step further -- the rule now reads as the same colour
    // as the words it marks, rather than a step fainter than them. `--nv-border` is still never used
    // here: it is `bg.mix(fg, 0.15)` and measures 1.27:1 on rose-pine dawn (`tokens.rs`'s own
    // derivation), and below the ramp's knee this 2px rule is still the ENTIRE cue that
    // distinguishes the two speakers -- a hairline that faint would make it disappear at exactly the
    // width where position alone hasn't kicked in yet. `--nv-border` is used instead for the NEW
    // turn-separator hairline below, a different edge of a different element, tested separately.
    const rule = withoutComments.match(/\.row-prompt \.row-body\s*\{[^}]*\}/);
    expect(rule).not.toBeNull();
    const body = rule![0];
    expect(body).toMatch(/margin-left:\s*clamp\(/);
    expect(body).toMatch(/border-left:\s*2px solid var\(--nv-fg\);/);
    expect(body).toMatch(/padding-left:\s*10px;/);
    expect(body).toMatch(/color:\s*var\(--nv-fg\);/);
    // Variant C also steps the prompt text down one size, to `--fs-base` (`.row-body`'s own base
    // rule declares `--fs-prose`), with a slightly tighter line-height. Text-matched for the same
    // reason as `border-left` just above -- keeping this rule's whole proof in one style rather than
    // splitting it between a text scan and a live cascade check for no real reason.
    expect(body).toMatch(/font-size:\s*var\(--fs-base\);/);
    expect(body).toMatch(/line-height:\s*1\.6;/);

    // `padding-left` and `color` are each a plain literal or a plain single-`var()` longhand --
    // neither shape gets dropped by this engine -- so the cascade half CAN be checked live for
    // those two: each has to actually win, not merely be declared somewhere in the file.
    const html = `<div class="message-list"><div class="row row-prompt"><div class="row-body">hi</div></div></div>`;
    const live = computed(html, ".row-prompt .row-body");
    expect(live.paddingLeft).toBe("10px");
    expect(live.color).toBe("var(--nv-fg)");
    expect(live.overflowWrap).toBe("anywhere");
    // Negative control: an equal-specificity rule placed AFTER the real one in source order wins by
    // the cascade's own tie-break, proving the assertions above are not vacuous.
    const later = computed(
      html,
      ".row-prompt .row-body",
      ".row-prompt .row-body { padding-left: 0px; color: var(--nv-muted); overflow-wrap: normal; }",
    );
    expect(later.paddingLeft).toBe("0px");
    expect(later.color).toBe("var(--nv-muted)");
    expect(later.overflowWrap).toBe("normal");

    // The border's cascade, checked live through the real CSSOM + `Element.matches` instead of
    // `getComputedStyle` (see `winningDeclaration`'s own doc for why that API cannot see this
    // property at all). The reviewer's own exact bypass: an ancestor-qualified selector at HIGHER
    // specificity ((0,3,0) vs (0,2,0)) cancelling the border while every text-match above stayed
    // satisfied, because nothing before this line ever asked who wins.
    expect(winningDeclaration(html, ".row-body", "border-left")).toBe("2px solid var(--nv-fg)");
    expect(
      winningDeclaration(html, ".row-body", "border-left", ".message-list .row-prompt .row-body { border-left: none; }"),
    ).not.toBe("2px solid var(--nv-fg)");

    // Round-2 review, exploit 1: `!important` beats specificity outright, so a LOWER-specificity
    // `.row-body { border-left: none !important; }` appended after the real rule still wins in a
    // real browser -- and a `winningDeclaration` that compared specificity/order alone, with no
    // notion of priority, missed it (that version returned the real rule's own text here, unchanged
    // by the bypass). This exact rule cancels the border in production if it is ever added, so
    // `winningDeclaration` has to say the shorthand no longer wins.
    expect(
      winningDeclaration(html, ".row-body", "border-left", ".row-body { border-left: none !important; }"),
    ).not.toBe("2px solid var(--nv-fg)");

    // Round-2 review, exploit 2: a rule that never spells the word "border-left" at all, only its
    // own LONGHANDS, at the SAME specificity and placed after the real rule -- a real cascade
    // reassembles the shorthand from whichever declaration wins each longhand, so this genuinely
    // cancels the visible border, but the old `winningDeclaration` asked the CSSOM for the literal
    // property `"border-left"`, which this rule never sets, so the exploit rule was invisible to it
    // and the real rule's text won by default. `winningDeclaration` now races the shorthand's own
    // longhands too and returns `null` -- not the shorthand's stale text -- when one of them wins
    // instead of the shorthand.
    expect(
      winningDeclaration(
        html,
        ".row-body",
        "border-left",
        ".row-prompt .row-body { border-left-style: none; border-left-width: 0; }",
      ),
    ).not.toBe("2px solid var(--nv-fg)");
  });

  it("puts .row-prompt .row-sign's colour before .row-current .row-sign in source order, so the cursor still wins", () => {
    // Global Constraint: `--nv-bg` as text only in the `.row-current .row-sign` rule -- so the new
    // prompt-row sign colour must never be able to beat the cursor's own colour. Both rules are
    // equal-specificity descendant selectors ((0,2,0) each: one class on the ancestor, one on
    // `.row-sign`), so on a row that is both `.row-prompt` and `.row-current` at once, the winner is
    // decided by which comes LAST in the file -- which means the prompt rule has to come first, not
    // merely "somewhere before the end of the file".
    const promptSignIndex = withoutComments.indexOf(".row-prompt .row-sign");
    const cursorSignIndex = withoutComments.indexOf(".row-current .row-sign");
    expect(promptSignIndex).toBeGreaterThan(-1);
    expect(cursorSignIndex).toBeGreaterThan(-1);
    expect(promptSignIndex).toBeLessThan(cursorSignIndex);

    const rule = withoutComments.match(/\.row-prompt \.row-sign\s*\{[^}]*\}/);
    expect(rule).not.toBeNull();
    expect(rule![0]).toMatch(/color:\s*var\(--nv-fg\);/);

    // Live, on a plain prompt row (not the cursor): the `›` reads --nv-fg now, not the base
    // `.row-sign` rule's --nv-muted.
    const plainHtml = `<div class="message-list"><div class="row row-prompt"><span class="row-sign">›</span><div class="row-body">hi</div></div></div>`;
    expect(computed(plainHtml, ".row-sign").color).toBe("var(--nv-fg)");

    // Live, where the row is BOTH .row-prompt and .row-current: the cursor's own --nv-bg still wins
    // the cascade tie, because it comes later in source order.
    const cursorHtml = `<div class="message-list"><div class="row row-prompt row-current"><span class="row-sign">›</span><div class="row-body">hi</div></div></div>`;
    expect(computed(cursorHtml, ".row-sign").color).toBe("var(--nv-bg)");

    // Negative control: an equal-specificity `.row-prompt .row-sign` rule appended AFTER the real
    // stylesheet (simulating a future edit that moves it past `.row-current .row-sign`) flips the
    // tie-break and steals the cursor's own colour -- proving the ordering assertion above is a real
    // constraint and not vacuously true.
    const reversed = computed(cursorHtml, ".row-sign", ".row-prompt .row-sign { color: var(--nv-fg); }");
    expect(reversed.color).toBe("var(--nv-fg)");
  });

  it("marks a turn boundary between prompts, and lets back-to-back prompts share one rule", () => {
    // The hairline here is a turn SEPARATOR, a different signal on a different edge of a different
    // element than `.row-prompt .row-body`'s own 2px `--nv-fg` speaker cue above -- so it uses
    // `--nv-border` (the line colour "uses --nv-border only for rules, never as a fill under text"
    // elsewhere in this file already guards), never the speaker cue's own colour, and the two cannot
    // be confused for one another.
    const notFirst = withoutComments.match(/\.row-prompt:not\(:first-child\)\s*\{[^}]*\}/);
    expect(notFirst).not.toBeNull();
    const notFirstBody = notFirst![0];
    expect(notFirstBody).toMatch(/margin-top:\s*10px;/);
    expect(notFirstBody).toMatch(/padding-top:\s*10px;/);
    expect(notFirstBody).toMatch(/border-top:\s*1px solid var\(--nv-border\);/);

    // Two prompts sent back to back (no reply between them) share one hairline, not two -- the
    // second prompt's own `:not(:first-child)` rule above would otherwise draw a second separator
    // directly under the first one's.
    const backToBack = withoutComments.match(/\.row-prompt \+ \.row-prompt\s*\{[^}]*\}/);
    expect(backToBack).not.toBeNull();
    const backToBackBody = backToBack![0];
    expect(backToBackBody).toMatch(/margin-top:\s*0;/);
    expect(backToBackBody).toMatch(/padding-top:\s*0;/);
    expect(backToBackBody).toMatch(/border-top:\s*none;/);
  });

  it("keeps every .row-prompt rule, not just .row-body, off the panel's bubble vocabulary", () => {
    // The two turn-separator rules just above are the first `.row-prompt`-selector rules that are
    // NOT `.row-prompt .row-body`/`.row-prompt .row-sign` -- this widens the "no fill, no frame, no
    // rounded corner" guard the dedicated `declarationsOnRealPromptBody` test below already holds
    // `.row-body` to, so a turn-separator rule cannot quietly grow into a card the same way that
    // test already stops `.row-body` from.
    const allPromptRules = rulesMatching(withoutComments, ".row-prompt");
    expect(allPromptRules.length).toBeGreaterThan(0);
    for (const { selector, body } of allPromptRules) {
      expect(body, `${selector} sets a background on a .row-prompt rule`).not.toMatch(/background/);
      expect(body, `${selector} sets a border-radius on a .row-prompt rule`).not.toMatch(/border-radius/);
    }
  });

  it("computes selector specificity the way this file's own comments already claim it, by hand", () => {
    // A sanity check on `specificity` itself, pinned against tuples this file's comments already
    // state without a mechanism behind them (the `:not(.row-choice)` guard above, and the
    // `[data-mode="input"]` mode-block override) -- so a defect in the calculator is caught here
    // rather than silently producing a wrong winner in the border-left cascade check above.
    expect(specificity(".mode-selector button:not(.row-choice)")).toEqual([0, 2, 1]);
    expect(specificity(".row")).toEqual([0, 1, 0]);
    expect(specificity(".row-choice")).toEqual([0, 1, 0]);
    expect(specificity(".row-prompt .row-body")).toEqual([0, 2, 0]);
    expect(specificity(".message-list .row-prompt .row-body")).toEqual([0, 3, 0]);
    // Panel round 2 (plan Task 10): `.band-mode[data-mode="input"]`, not `.panel-footer .mode-block
    // [data-mode="input"]` -- one class plus one attribute now that the wrapper prefix is gone.
    expect(specificity('.band-mode[data-mode="input"]')).toEqual([0, 2, 0]);
    expect(compareSpecificity([0, 3, 0], [0, 2, 0])).toBeGreaterThan(0);
  });

  /** Every declaration ANY rule in `css${extraCss}` sets on the REAL prompt-row body element --
   *  exactly what `Row.tsx` renders for `kind="prompt"` (a `div.row.row-prompt[data-sign]` holding
   *  `div.row-sign[aria-hidden]` and `div.row-body`), matched through the browser's own
   *  `Element.matches` over parsed CSSOM, never by scanning selector TEXT for the substring
   *  `.row-prompt`.
   *
   *  Round-2 review, exploit 1: the guard used to be `rulesMatching(sheet, ".row-prompt")`, which
   *  finds a rule only if that literal substring appears somewhere in its selector text. Two real
   *  selectors reach the exact same element and neither one spells it:
   *  `[class~="row-prompt"] .row-body` (an attribute selector, not a class selector, so the substring
   *  `.row-prompt` never appears in it), and `.message-list > div:first-child .row-body` (the prompt
   *  row is always the FIRST child of `.message-list` in the fixture below, so this reaches it
   *  through pure structural position and never names `.row-prompt` either). Both are real, valid
   *  CSS that a real browser resolves against the real element; a text scan for one specific class
   *  name is not a stand-in for "does this rule apply to this element", and this function is what
   *  actually asks that question. */
  function declarationsOnRealPromptBody(extraCss = ""): { selector: string; property: string; value: string }[] {
    const html =
      '<div class="message-list"><div class="row row-prompt" data-sign="›">' +
      '<div class="row-sign" aria-hidden="true">›</div><div class="row-body">hi</div></div></div>';
    document.head.innerHTML = `<style>${css}${extraCss}</style>`;
    document.body.innerHTML = html;
    const el = document.body.querySelector(".row-prompt .row-body");
    if (el === null) throw new Error("fixture does not contain a .row-prompt .row-body");
    const sheet = document.styleSheets[0];
    const out: { selector: string; property: string; value: string }[] = [];
    for (const rule of allStyleRules(sheet.cssRules)) {
      for (const branch of rule.selectorText.split(",").map((b) => b.trim())) {
        let matches = false;
        try {
          matches = el.matches(branch);
        } catch {
          matches = false;
        }
        if (!matches) continue;
        // `rule.style.cssText`, not `.item()`/`.length`: the raw declared text, which (per
        // `winningDeclaration`'s own doc above) keeps a `var()`-bearing shorthand's spelling intact
        // rather than dropping it, exactly like the property-family scan this replaces used to read
        // straight off rule-body text.
        for (const decl of rule.style.cssText.split(";")) {
          const colon = decl.indexOf(":");
          if (colon === -1) continue;
          out.push({ selector: branch, property: decl.slice(0, colon).trim().toLowerCase(), value: decl.slice(colon + 1).trim() });
        }
      }
    }
    return out;
  }

  it("keeps .row-prompt off the panel's bubble vocabulary: no fill, no frame, no rounded corner, no fit-content box", () => {
    // The panel's real boxes are `.tool-card`/`.permission-card` (an `--nv-surface` fill and a
    // radius, `width: fit-content` on nothing); the user's own message is meant to read as an
    // indent, exactly as far from those as it is from Cursor's own bubble -- `border-radius: 12px`
    // on a bordered, FILLED, `width: fit-content` box (index.css's own comment names the exact
    // rule, read out of Cursor's own stylesheet). Stated as a test because it is a design decision
    // a future edit could otherwise quietly undo one property at a time, and it is this file's
    // single strongest link to the owner's own headline constraint for this row
    // ("参考实现，同时不要让别人一看就觉得是cursor" -- take the reference implementation's idea without
    // it reading as a copy of Cursor's own look).
    //
    // A prior version of this guard matched two literal property SPELLINGS (`background`/
    // `background-color`, `border-radius`) inside rules found by a selector-TEXT scan for the
    // substring `.row-prompt`, so this passed green (found by the round-1 review):
    //   background-image: linear-gradient(var(--nv-surface), var(--nv-surface));
    //   border-top-left-radius: 12px; border-top-right-radius: 12px;
    //   border-bottom-right-radius: 12px; border-bottom-left-radius: 12px;
    //   width: fit-content;
    // -- a filled, fully rounded, fit-content pill: literally the bubble this row must not become.
    // Round 1 widened the PROPERTY match to whole families (`background*`, every `border*-radius`
    // corner). Round 2 found that widening the property match was not enough while the SELECTOR
    // match still scanned text: `[class~="row-prompt"] .row-body { <the same pill> }` and
    // `.message-list > div:first-child .row-body { <the same pill> }` both apply to the real element
    // and neither contains the substring `.row-prompt`, so the whole rule was invisible to the scan
    // regardless of which properties it set. This guard now finds every rule that MATCHES the real
    // element (`declarationsOnRealPromptBody` above), so no spelling of the selector can hide a rule
    // from it -- only the property vocabulary matters, which is what a bubble actually needs.
    //
    // Round 2 also found a second gap in the property vocabulary itself: `box-shadow: inset 0 0 0
    // 99px var(--nv-surface)` paints the exact same solid fill as a `background` would, from inside
    // the border box, and `outline` can draw the exact same frame a `border-radius`-cornered box
    // does without ever touching `border` or `background` at all. Both are in the vocabulary now.
    // **This is a fence around a known vocabulary of "fill or frame a box", not a proof that no
    // combination of CSS properties could ever look like a bubble** -- a sufficiently creative
    // `clip-path`, `mask`, `filter`, or a pseudo-element carrying its own box are not covered, and
    // nothing here claims they are.
    const declarations = declarationsOnRealPromptBody();
    expect(declarations.length).toBeGreaterThan(0);
    for (const { selector, property, value } of declarations) {
      expect(
        property,
        `${selector} sets ${property} on the real prompt-row body, part of the panel's bubble vocabulary (the background family)`,
      ).not.toMatch(/^background/);
      expect(
        property,
        `${selector} sets ${property} on the real prompt-row body, a rounded corner -- the bubble's own signature shape`,
      ).not.toMatch(/^border(-[a-z]+)*-radius$/);
      expect(
        property,
        `${selector} sets ${property} on the real prompt-row body -- a fill or a frame, the same bubble vocabulary as background/border-radius`,
      ).not.toMatch(/^(box-shadow|outline(-[a-z]+)?)$/);
      if (property === "width") {
        expect(value, `${selector} sizes the real prompt-row body width: fit-content, a bubble's own box sizing`).not.toMatch(
          /fit-content/,
        );
      }
    }

    // Negative control 1 (round-2 review, exploit 1a): the attribute-selector bypass, reproduced
    // against the SAME matcher the real guard above uses. `.not.toEqual([])` rather than a thrown
    // error is the point -- the OLD (`rulesMatching`) guard's for-loop would have run zero times over
    // this rule and reported nothing wrong; this one actually finds the fill.
    const attrBypass = declarationsOnRealPromptBody(
      '[class~="row-prompt"] .row-body { background: var(--nv-surface); border-radius: 12px; width: fit-content; }',
    );
    expect(attrBypass.some((d) => /^background/.test(d.property))).toBe(true);

    // Negative control 2 (round-2 review, exploit 1b): the structural-position bypass -- the prompt
    // row is the fixture's first (and only) child of `.message-list`, so this reaches it with no
    // class name at all beyond `.row-body`.
    const structuralBypass = declarationsOnRealPromptBody(
      ".message-list > div:first-child .row-body { background: var(--nv-surface); border-radius: 12px; }",
    );
    expect(structuralBypass.some((d) => /^background/.test(d.property))).toBe(true);

    // Negative control 3 (round-2 review, exploit 2): the box-shadow fill -- a real rule, matched by
    // the real `.row-prompt .row-body` selector this file already uses, that neither `background` nor
    // `border-radius` alone could catch.
    const boxShadowBypass = declarationsOnRealPromptBody(
      ".row-prompt .row-body { box-shadow: inset 0 0 0 99px var(--nv-surface); }",
    );
    expect(boxShadowBypass.some((d) => d.property === "box-shadow")).toBe(true);
  });

  it("keeps the sign column's own track for a prompt row, and gives .row-prompt .row-sign no margin or padding", () => {
    // The prompt row's inset lives on `.row-body` alone. The sign column is the spine the whole
    // document lines up on (the same point the "keeps the sign column on the same track..." test
    // above makes for a wide code block) and must never move for one row TYPE either.
    const promptRow =
      `<div class="message-list"><div class="row row-prompt">` +
      `<span class="row-sign">›</span><div class="row-body">hi</div></div></div>`;
    const assistantRow =
      `<div class="message-list"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body">hi</div></div></div>`;
    // Positive control: a real, non-empty `grid-template-columns` value is what makes the equality
    // below mean something. Against a ZERO-BYTE stylesheet `.row` never applies at all, both sides
    // compute to the same default ("none"), and the assertion would pass vacuously -- pinning the
    // real value closes that, since "none" fails to equal it.
    const columns = expandVars(computed(promptRow, ".row").gridTemplateColumns);
    expect(columns).toBe("22px minmax(0, 1fr)");
    expect(columns).toBe(expandVars(computed(assistantRow, ".row").gridTemplateColumns));

    // Positive control 2: the rules this scan is about to walk actually exist in the real file. An
    // empty result here would let the loop below run zero times and pass by doing nothing -- which
    // is exactly what happened against a zero-byte stylesheet before this control was added.
    const promptRules = rulesMatching(withoutComments, ".row-prompt");
    expect(promptRules.length).toBeGreaterThan(0);

    const signRules = promptRules.filter(({ selector }) => selector.includes(".row-sign"));
    for (const { body } of signRules) {
      expect(body).not.toMatch(/margin(-[a-z]+)?\s*:/);
      expect(body).not.toMatch(/padding(-[a-z]+)?\s*:/);
    }
    // Positive control 3: if such a rule existed, computed() would actually see it, so an empty
    // `signRules` above cannot make this test pass for free.
    const withMargin = computed(promptRow, ".row-sign", ".row-prompt .row-sign { margin-left: 4px; }");
    expect(withMargin.marginLeft).toBe("4px");
  });

  it("builds the prompt inset from the row's own width, floored at 0, out of the real four knobs", () => {
    // `--prompt-inset` itself no longer exists -- it was replaced by four knobs
    // (`--prompt-inset-knee`/`-slope`/`-cap`/`-cap-max`) that `.row-prompt .row-body`'s own
    // `margin-left` combines inline via `clamp(...)`. This guard is re-pointed at the real thing.
    for (const token of ["--prompt-inset-knee", "--prompt-inset-slope", "--prompt-inset-cap", "--prompt-inset-cap-max"]) {
      expect(ROOT_TOKENS.has(token), `${token} is not declared -- the inset ramp lost one of its four knobs`).toBe(
        true,
      );
    }
    // Round-3 review: the line above used to be a NON-GLOBAL first-match text scan for
    // `.row-prompt .row-body { ... }`, which reads only the FIRST such block in the file. A later
    // duplicate rule -- or a `margin-inline-start` carrying the same percentages -- restored the
    // percentage-of-grid-area basis while all four token names stayed spelled in the dead rule
    // above, and this stayed green. So the displacement is now collected from EVERY rule that
    // really matches a prompt-row body (including inside `@media`, which the walkers were blind to
    // until the same review), and every one of them has to be the row-based ramp. That is stricter
    // than asking who wins the cascade, deliberately: a percentage basis that only applies at some
    // widths is still a percentage basis.
    const displacements = declarationsOnRealPromptBody().filter(
      (d) => d.property === "margin-left" || d.property === "margin-inline-start" || d.property === "margin",
    );
    expect(
      displacements.length,
      "no rule matching a real prompt-row body declares a displacement at all",
    ).toBeGreaterThan(0);
    for (const d of displacements) {
      expect(d.value, `${d.selector} displaces the prompt with ${d.value}, which is not the clamp ramp`).toMatch(
        /^clamp\(/,
      );
      expect(
        expandRowVars(d.value),
        `${d.selector} resolves to ${expandRowVars(d.value)}, which measures a PERCENTAGE of the grid area`,
      ).not.toMatch(/%/);
    }
    const clamp = displacements[0].value;

    // The MIN term has to be exactly 0 so a narrow panel gives the space back to the text instead
    // of reserving it (index.css's own stated intent) -- a non-zero floor would keep eating width
    // on the narrowest panels no matter how narrow they get.
    const min = clamp.match(/^clamp\(\s*([^,]+),/);
    expect(min).not.toBeNull();
    expect(min![1].trim()).toBe("0px");

    // The SHAPE: all four real knobs are actually spent, not just declared and left unused.
    for (const token of ["--prompt-inset-knee", "--prompt-inset-slope", "--prompt-inset-cap", "--prompt-inset-cap-max"]) {
      expect(clamp, `${clamp} does not spend var(${token})`).toContain(`var(${token})`);
    }
    // The BASIS is the ROW, never a percentage of the grid AREA. A percentage on a grid item
    // resolves against `.row-body`'s own track, which depends on `--prose-measure`, so the inset
    // would silently move whenever prose width changed -- measured: `62ch` made the area 496.5px,
    // 3.5px under a 500px knee, and the displacement vanished at every width with nothing to show
    // for it. The row was `100cqw` until 2026-09-24 and is `var(--row-inline-size)` now, `.row`'s
    // own width derived from the list's measured one (no query container: see the tripwire near the
    // top of the `index.css` block for why).
    //
    // Round-2 review: checking the clamp's own TEXT for the row basis and for the absence of `%` is
    // beaten by hiding the percentage behind a token and padding a dead term to satisfy the basis
    // match -- `(<row> * 0) + (100% - 5000px) * 3` contains the basis AND contains no `%` if the
    // `100%` is written as `var(--some-basis)` instead, reintroducing exactly the
    // percentage-of-grid-area basis this guard exists to prevent while reading green on both checks.
    // Fixed by expanding every `var()` against the real declarations FIRST (`expandRowVars`: `:root`
    // plus `.row`'s own derived width, since jsdom does not resolve `var()` either), and asking the
    // expanded text these questions instead of the raw one.
    const expandedClamp = expandRowVars(clamp);
    expect(expandedClamp).toMatch(/var\(--list-inline-size, 0px\)/);
    expect(expandedClamp).not.toMatch(/%/);
    expect(expandedClamp).not.toMatch(/\dcq(w|h|i|b|min|max)\b/i);

    // As numbers, which is the claim itself: nothing until the ROW is past the 560px knee, three
    // tenths of the surplus after it, capped at 8% of the row and never above 120px, and 0 before
    // the list has been measured at all. The percent basis is passed as something absurd on
    // purpose -- the grid area must not enter, so it must not matter.
    const inset = (listInlineSize: number | null) => evaluatePx(clamp, listInlineSize, 99999);
    expect(inset(null)).toBe(0);
    expect(inset(400)).toBe(0); // row 398px, under the knee
    expect(inset(562)).toBe(0); // row 560px, exactly at the knee
    expect(inset(700)).toBeCloseTo(41.4, 6); // row 698px: min((698 - 560) * 0.3, 8% of 698) = 41.4
    expect(inset(1000)).toBeCloseTo(79.84, 6); // row 998px: the 8% cap (79.84) beats 131.4
    expect(inset(2000)).toBe(120); // row 1998px: the absolute cap
    expect(evaluatePx(clamp, 700, 1)).toBe(inset(700)); // ...and the grid area really does not enter

    // Negative control, the reviewer's own bypass: `clamp(0px,(100% - 5000px)*3,80%)` still LOOKS
    // plausible -- a 0 floor, a clamp shape -- but is expressed in percentages of the grid area
    // rather than the row's width, which is exactly the defect the basis correction above fixed.
    // A guard that only checked "floors at 0" and "has a % somewhere" (the pre-fix version) let
    // this straight through; this one fails it on the missing tokens, the missing row basis, and
    // the grid area entering the number.
    const bypass = "clamp(0px,(100% - 5000px)*3,80%)";
    expect(expandRowVars(bypass)).not.toMatch(/--list-inline-size/);
    for (const token of ["--prompt-inset-knee", "--prompt-inset-slope", "--prompt-inset-cap", "--prompt-inset-cap-max"]) {
      expect(bypass).not.toContain(`var(${token})`);
    }
    expect(evaluatePx(bypass, 700, 10000)).not.toBe(evaluatePx(bypass, 700, 20000));

    // Negative control 2, the round-2 bypass itself: hides the `%` behind a token (`--prompt-basis`
    // does not exist in the real file and is never added just to prove this) and pads a dead
    // `<row> * 0` term. This is exactly what the OLD, text-only checks would have let through --
    // demonstrated by hand-inlining the one substitution `expandRowVars` performs, since there is
    // no real declaration for `--prompt-basis` to expand it against.
    const hiddenPercentBypass = "clamp(0px, (var(--row-inline-size) * 0) + (var(--prompt-basis) - 5000px) * 3, 120px)";
    const hiddenPercentBypassExpanded = hiddenPercentBypass.replace("var(--prompt-basis)", "100%");
    expect(hiddenPercentBypass).toMatch(/var\(--row-inline-size\)/); // <- the OLD "has the row basis" check, satisfied wrongly
    expect(hiddenPercentBypass).not.toMatch(/%/); // <- the OLD "has no %" check, ALSO satisfied wrongly
    expect(hiddenPercentBypassExpanded).toMatch(/%/); // <- expanding first is what actually catches it
  });

  // "keeps a choice row off the boxed-button look" (the `.mode-selector button:not(.row-choice)`
  // vs. `.row-choice` specificity regression) was removed here on 2026-09-25, session tabs Task 9:
  // the mode-selector start screen's boxed Auto/Bypass buttons are gone along with the screen
  // itself (the empty tab, F3, never draws a boxed button), so `index.css` no longer carries that
  // rule at all -- there is nothing left for `.row-choice` to lose a specificity fight WITH. The
  // positive control this test opened with (`.mode-selector button:not(.row-choice)` must exist in
  // the real file) would now fail correctly, on a feature that was deliberately deleted rather than
  // regressed. `.row-choice`'s own rules (padding, background: none, the `:hover`/`:selected`
  // fills) are unaffected and still covered by the tests around this one.

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
    expect(computed(MODE_BLOCK, ".band-mode").borderLeftColor).toBe("var(--nv-mode-input)");
    const clobbered = computed(
      MODE_BLOCK,
      ".band-mode",
      ".band-mode { border-left: 3px solid var(--nv-mode-browse); }",
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

  /* K07 (2026-09-29): a HINT landing on a code block is "the item" until the next key (`y` copies it,
     `v` starts inside it), and it is now drawn: the block outlined in the body's ink, thinner while the
     panel does not have the keys, and the row's sign hollow meanwhile -- one solid mark at a time, as
     for a control focused inside the current row. */
  it("K07: outlines a HINT-landed code block, thinner without the keys, and hollows the row's sign", () => {
    const landed = (focused: string) =>
      `<div class="message-list" data-focused="${focused}" data-code-landed=""><div class="row row-assistant row-current">` +
      `<span class="row-sign">●</span><div class="row-body"><pre class="code-block" data-hint-landed=""><code>x</code></pre></div></div></div>`;
    const block = computed(landed("true"), "pre.code-block");
    expect(block.outlineStyle).toBe("solid");
    expect(block.outlineWidth).toBe("2px");
    expect(block.outlineColor).toBe("var(--nv-fg)");
    expect(computed(landed("false"), "pre.code-block").outlineWidth).toBe("1.5px");
    const sign = computed(landed("true"), ".row-sign");
    expect(sign.background).not.toBe("var(--nv-fg)");
    expect(sign.color).toBe("var(--nv-fg)");
    expect(sign.boxShadow).toBe("inset 0 0 0 1.5px var(--nv-fg)");
    // Not landed: no outline, and the sign is the solid cursor.
    const plain =
      `<div class="message-list" data-focused="true"><div class="row row-assistant row-current">` +
      `<span class="row-sign">●</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    expect(computed(plain, "pre.code-block").outlineStyle).not.toBe("solid");
    expect(computed(plain, ".row-sign").background).toBe("var(--nv-fg)");
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
    // `[data-mode="input"]` and `[data-focused="false"]` are both (0,2,0), so source order decides
    // and the unfocused rule must come second. Both sides are `var()` longhands, the combination
    // this engine resolves correctly (see the big comment above).
    const block = computed(UNFOCUSED_INPUT_BLOCK, ".band-mode");
    expect(block.borderLeftColor).toBe("var(--nv-muted)");
    expect(block.color).toBe("var(--nv-muted)");
    // A focused block keeps its per-mode rule and inherits the bright label colour.
    const focused = computed(MODE_BLOCK.replace('data-mode="input"', 'data-mode="input" data-focused="true"'), ".band-mode");
    expect(focused.borderLeftColor).toBe("var(--nv-mode-input)");
    expect(focused.color).not.toBe("var(--nv-muted)");
    // Negative control: the INPUT rule re-declared after the unfocused one wins it back. That is
    // what swapping the two rules in index.css would do.
    const clobbered = computed(
      UNFOCUSED_INPUT_BLOCK,
      ".band-mode",
      '.band-mode[data-mode="input"] { border-left-color: var(--nv-mode-input); }',
    );
    expect(clobbered.borderLeftColor).toBe("var(--nv-mode-input)");
  });

  /** Spec §6.1, the r2-gui GUI pass (2026-09-26): the chooser's current row carries the panel's
   *  solid cursor in its sign cell, and a record's long title stays on its own line beside the sign,
   *  cut, rather than wrapping under it (it dropped to the row's left edge, under the sign column). */
  it("draws the chooser's cursor solid and keeps a long title beside its sign", () => {
    const html = `<div class="chooser"><div class="chooser-row current"><span class="chooser-sign">›</span><span class="chooser-line1"><span class="chooser-lead">t</span><span class="chooser-right">12 min ago</span></span></div></div>`;
    const sign = computed(html, ".chooser-sign");
    expect(sign.background).toBe("var(--nv-fg)");
    expect(sign.color).toBe("var(--nv-bg)");
    const line1 = computed(html, ".chooser-line1");
    expect(line1.flexBasis).toMatch(/^0(%|px)?$/);
    const lead = computed(html, ".chooser-lead");
    expect(lead.whiteSpace).toBe("nowrap");
    expect(lead.overflow).toBe("hidden");
    expect(lead.textOverflow).toBe("ellipsis");
  });

  /** `mock: bottom.html` B: `✻ Working… 1m 12s · ctrl+c interrupt` -- the Stop control is words in
   *  the line (spec §5.1), not a boxed button (the r2-gui GUI pass, 2026-09-26, saw WebKit's own
   *  button face). Focused, it is still the solid cursor every focused control is. */
  it("draws the activity line's Stop as words, and as the solid cursor when focused", () => {
    const html = `<div class="agent-ui-root"><div class="activity-line"><button type="button" class="stop">ctrl+c interrupt</button></div></div>`;
    const stop = computed(html, ".stop");
    expect(stop.borderTopStyle).toMatch(/^(none|)$/);
    expect(stop.backgroundColor).toMatch(/^(transparent|rgba\(0, 0, 0, 0\)|)$/);
    expect(stop.color).toBe("var(--nv-muted)");
    // A fresh copy, focused before its style is first read: jsdom does not recompute on focus.
    document.body.innerHTML = html;
    document.body.querySelector<HTMLElement>(".stop")!.focus();
    const now = getComputedStyle(document.body.querySelector(".stop")!);
    expect(now.background).toBe("var(--nv-fg)");
    expect(now.color).toBe("var(--nv-bg)");
  });

  /** `mock: bottom.html` B: a bare `❯` line in both modes (spec §5.1). The r2-gui GUI pass
   *  (2026-09-26) saw BROWSE's stand-in still boxed, and WebKit's own focus ring drawing a box round
   *  the textarea in INPUT. */
  it("draws the composer as a bare line in both modes: no box, no focus ring", () => {
    const hint = computed(`<div class="composer"><div class="composer-browse-hint">x</div></div>`, ".composer-browse-hint");
    for (const side of ["Top", "Right", "Bottom", "Left"] as const) {
      expect(hint[`border${side}Style` as "borderTopStyle"], side).toMatch(/^(none|)$/);
    }
    expect(hint.borderRadius).toMatch(/^(0|0px|)$/);
    const box = computed(`<div class="composer"><textarea></textarea></div>`, ".composer textarea");
    expect(box.outlineStyle).toBe("none");
  });

  /* The v1-ui GUI pass (2026-09-27): WebKitGTK drew "Ask the agent...— i or Ctrl+j to type" -- the
     hint span is a flex item of `.composer-browse-hint`, and a flex item's leading white space is
     removed, so the space `Composer.tsx` puts before the dash never showed. jsdom has no layout; this
     pins the rule that keeps it. */
  it("keeps the space before the BROWSE hint's dash (a flex item drops its leading white space)", () => {
    const hint = computed(
      `<div class="composer"><div class="composer-browse-hint">Ask the agent...<span class="composer-hint"> — i or Ctrl+j to type</span></div></div>`,
      ".composer-hint",
    );
    expect(hint.whiteSpace).toMatch(/^pre(-wrap)?$/);
  });

  /* The v1-ui GUI pass (2026-09-27): the missing-sidecar remedy ("Looked for it at:\n  - <path>\n
     Install ...") ran together into one line. */
  it("keeps a problem remedy's own line breaks", () => {
    const remedy = computed(`<div class="row-problem"><div class="row-problem-remedy">a\nb</div></div>`, ".row-problem-remedy");
    expect(remedy.whiteSpace).toMatch(/^pre(-line|-wrap)?$/);
  });

  it("gives the composer the panel's own font and caps its growth", () => {
    const box = computed(`<div class="composer"><textarea></textarea></div>`, ".composer textarea");
    expect(box.fontFamily).not.toMatch(/-webkit-small-control/);
    // jsdom resolves `40vh` against `window.innerHeight` rather than reporting the token verbatim
    // (unlike a `var()`, which every other test in this file checks unresolved) -- deviation from
    // the brief's literal `toBe("40vh")`, recorded in the task report. `parseFloat`, not a string
    // comparison against `window.innerHeight * 0.4`: the two float multiplications round
    // differently (`307.2` here vs `307.20000000000005` in plain JS arithmetic).
    expect(box.maxHeight.endsWith("px")).toBe(true);
    expect(Number.parseFloat(box.maxHeight)).toBeCloseTo(window.innerHeight * 0.4, 5);
    expect(box.overflowY).toBe("auto");
    const reason = computed(`<div class="permission-card"><input type="text"></div>`, ".permission-card input");
    expect(reason.lineHeight).toBe("1.4");
  });

  /** Defect 3 (phase 2's sandbox pass): at ~348px beside a Lua panel the empty tab scrolled
   *  sideways -- `width: 100%` plus 48px of padding in a content-box. */
  it("keeps the empty tab inside its column", () => {
    const tab = computed(`<div class="agent-ui-root"><div class="empty-tab">x</div></div>`, ".empty-tab");
    expect(tab.boxSizing).toBe("border-box");
    expect(tab.minWidth).toBe("0px");
    const title = computed(`<div class="empty-tab-resume"><strong class="session-title">t</strong></div>`, ".session-title");
    expect(title.overflowWrap).toBe("anywhere");
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
/* The sandbox GUI pass (2026-09-24), its finding F-b: a long `Bash` command scrolled the whole
   conversation sideways -- 517px at a 420px panel, identical on `main` -- because `toolRegistry.tsx`
   renders it as a `<pre>` and no rule ever took that `<pre>` off the UA's `white-space: pre`, and
   `.message-list` scrolls on both axes. A WebKitGTK probe then found every other tool card (a path,
   a URL, a pattern with no space in it), a URL in a reply's prose and a markdown table doing the same.
   jsdom has no layout, so what these pin is the stylesheet's half of the contract: every line in the
   list may break where it has to, and every `<pre>` the panel renders there either wraps or scrolls in
   its own box. `shell/tests/panel_stream_scroll.rs` measures the result in the real engine. Both
   tests fail on `8e58403`. */
describe("index.css: nothing in the conversation scrolls it sideways", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  it("lets any line in the list break where it has to, inherited from .message-list", () => {
    const html = `<div class="message-list"><div class="row row-assistant"><div class="row-body"><p>x</p></div></div></div>`;
    expect(winningDeclaration(html, ".message-list", "overflow-wrap")).toBe("anywhere");
    // Negative control: the declaration can lose, and this reads the loser as such.
    expect(winningDeclaration(html, ".message-list", "overflow-wrap", ".message-list { overflow-wrap: normal; }")).toBe(
      "normal",
    );
    // And nothing between the list and a paragraph takes it back.
    for (const selector of [".row", ".row-body", "p"]) {
      expect(winningDeclaration(html, selector, "overflow-wrap")).toBeNull();
    }
  });

  /** Every `<pre>` the panel's own renderers put in a row: a `Bash` call with its result, an
   *  unrecognized tool's JSON dump, a fenced code block, and a permission card's input and diff. */
  function panelPres(extraCss = ""): HTMLElement[] {
    const bash = renderToolCall({
      seq: 1,
      toolUseId: "t1",
      name: "Bash",
      input: { command: "cargo test" },
      result: { content: "ok", isError: false },
    });
    const generic = renderToolCall({ seq: 2, toolUseId: "t2", name: "mcp__demo__lookup", input: { k: "v" }, result: null });
    const card = (toolName: string, input: unknown) =>
      createElement(PermissionCard, {
        request: { seq: 3, permissionId: `p-${toolName}`, toolUseId: null, toolName, input },
        sessionEnded: false,
        onAnswer: () => {},
      });
    const rows = [
      renderToStaticMarkup(createElement("div", null, bash)),
      renderToStaticMarkup(createElement("div", null, generic)),
      renderMarkdown("```rust\nfn main() {}\n```"),
      renderToStaticMarkup(card("Bash", { command: "cargo test" })),
      renderToStaticMarkup(card("Edit", { file_path: "/p/a.rs", old_string: "a\n", new_string: "b\n" })),
      // Task 14 (P4): a Bash card now renders `.permission-card-command`, not `.permission-card-input`
      // -- this row is what keeps the raw-JSON class in the fixture, for a tool with no dedicated view.
      renderToStaticMarkup(card("mcp__demo__lookup", { k: "v" })),
    ];
    document.head.innerHTML = `<style>${css}${extraCss}</style>`;
    document.body.innerHTML =
      `<div class="message-list">` +
      rows.map((row) => `<div class="row"><span class="row-sign"></span><div class="row-body">${row}</div></div>`).join("") +
      `</div>`;
    return Array.from(document.querySelectorAll<HTMLElement>(".message-list pre"));
  }

  /** Why a `<pre>` cannot push the list sideways, or `null` when nothing stops it. No winning
   *  `white-space` means the UA's own `pre`. */
  function containment(pre: HTMLElement): string | null {
    const whiteSpace = winningDeclarationOn(pre, "white-space");
    if (whiteSpace !== null && whiteSpace !== "pre" && whiteSpace !== "nowrap") return `wraps (white-space: ${whiteSpace})`;
    for (const property of ["overflow-x", "overflow"]) {
      const value = winningDeclarationOn(pre, property);
      if (value !== null && /\b(?:auto|scroll|hidden|clip)\b/.test(value)) return `scrolls in its own box (${property}: ${value})`;
    }
    return null;
  }

  it("wraps or scrolls every <pre> the panel renders in a row, never leaving one at the UA's white-space: pre", () => {
    const pres = panelPres();
    // The fixture really holds all six, so a renderer that stopped emitting one cannot pass by absence.
    const kinds = pres.map((pre) => pre.className || "(no class)").sort();
    expect(kinds).toEqual(
      [
        "(no class)",
        "code-block",
        "permission-card-command",
        "permission-card-diff",
        "permission-card-input",
        "tool-card tool-card-bash",
        "tool-result-body",
      ].sort(),
    );
    for (const pre of pres) {
      expect({ pre: pre.className || pre.parentElement?.className, how: containment(pre) }).not.toMatchObject({ how: null });
    }
    // Negative control: the `Bash` command's own rule taken away is exactly the GUI pass's defect.
    const bash = panelPres(".tool-card-bash { white-space: pre; }").find((pre) => pre.classList.contains("tool-card-bash"))!;
    expect(containment(bash)).toBeNull();
  });

  /** A folded call with a long first line, as the panel draws it: the preview under its invocation. */
  function foldedPreview(extraCss = "", isError = false): HTMLElement {
    const folded = renderToolCall(
      {
        seq: 1,
        toolUseId: "t1",
        name: "Bash",
        input: { command: "cargo test" },
        result: { content: `${"x".repeat(300)}\nsecond\nthird\nfourth`, isError },
      },
      false,
    );
    document.head.innerHTML = `<style>${css}${extraCss}</style>`;
    document.body.innerHTML =
      `<div class="message-list"><div class="row"><span class="row-sign"></span><div class="row-body">` +
      renderToStaticMarkup(createElement("div", null, folded)) +
      `</div></div></div>`;
    return document.querySelector<HTMLElement>(".tool-result-preview")!;
  }

  it("cuts a folded result's lines at the edge with an ellipsis, so three lines stay three lines", () => {
    const line = foldedPreview().querySelector<HTMLElement>(".tool-result-preview-line")!;
    expect(winningDeclarationOn(line, "white-space")).toBe("pre");
    expect(winningDeclarationOn(line, "overflow")).toBe("hidden");
    expect(winningDeclarationOn(line, "text-overflow")).toBe("ellipsis");
    // Negative control: without the clip the line is free to push the row wide.
    const loose = foldedPreview(".tool-result-preview-line { overflow: visible; }").querySelector<HTMLElement>(
      ".tool-result-preview-line",
    )!;
    expect(winningDeclarationOn(loose, "overflow")).toBe("visible");
  });

  it("holds the preview's lines in one zero-minimum track, which is what keeps a long line from widening the row", () => {
    const preview = foldedPreview();
    expect(winningDeclarationOn(preview, "display")).toBe("grid");
    expect(winningDeclarationOn(preview, "grid-template-columns")).toBe("minmax(0, 1fr)");
  });

  it("draws the preview's count in the muted text colour and a failure beside the error rule, not in it", () => {
    const ok = foldedPreview();
    expect(winningDeclarationOn(ok.querySelector(".tool-result-preview-more")!, "color")).toBe("var(--nv-muted)");
    const failed = foldedPreview("", true);
    expect(failed.classList.contains("tool-result-error")).toBe(true);
    expect(winningDeclarationOn(failed, "border-left")).toBe("2px solid var(--nv-error)");
    expect(winningDeclarationOn(failed, "color")).toBe("var(--nv-fg)");
  });

  it("scrolls a markdown table in its own box and keeps its words whole (T1)", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = `<div class="message-list"><div class="row"><div class="row-body">${renderMarkdown("| a | b |\n|---|---|\n| supercalifragilistic | 2 |")}</div></div></div>`;
    const wrapper = document.querySelector<HTMLElement>(".table-scroll")!;
    expect(winningDeclarationOn(wrapper, "overflow-x")).toBe("auto");
    const cell = document.querySelector<HTMLElement>(".table-scroll td")!;
    expect(winningDeclarationOn(cell, "overflow-wrap")).toBe("normal");
  });
});

/* The keymap GUI pass (2026-09-25): at the default 520px panel the `?` overlay scrolled sideways,
   and at 348px (beside a Lua side panel) every row below the window keys showed its keys and no
   description at all, pushed off the right edge. The keys column was `white-space: nowrap`, and one
   key cell -- `Ctrl+0 / Ctrl+Keypad0 / Ctrl+Keypad0 (NumLock off)`, the same text on `main` -- is
   wider than the panel. The keys may break at their own spaces; a key name itself never does,
   because nothing here sets `overflow-wrap` on the overlay. */
describe("index.css: the ? overlay never scrolls sideways", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  function keyCells(extraCss = ""): HTMLElement[] {
    const long = { keys: "Ctrl+0 / Ctrl+Keypad0 / Ctrl+Keypad0 (NumLock off)", what: "Text size reset (both panes)" };
    document.head.innerHTML = `<style>${css}${extraCss}</style>`;
    document.body.innerHTML = renderToStaticMarkup(
      createElement(KeymapOverlay, {
        onClose: () => {},
        windowKeys: [long],
        prefixKeys: [],
        prefixLabel: "Ctrl+b",
        panel: EMPTY_PANEL_TABLE,
      }),
    );
    return Array.from(document.querySelectorAll<HTMLElement>(".keymap-overlay td:first-child"));
  }

  it("lets a key cell wrap at its spaces", () => {
    const cells = keyCells();
    expect(cells.length).toBeGreaterThan(10);
    for (const cell of cells) {
      expect(winningDeclarationOn(cell, "white-space") ?? "normal").toBe("normal");
      expect(winningDeclarationOn(cell.querySelector(".keycap")!, "white-space") ?? "normal").toBe("normal");
    }
    // Negative control: the rule the pass found reads as the defect.
    expect(winningDeclarationOn(keyCells(".keymap-overlay td:first-child { white-space: nowrap; }")[0], "white-space")).toBe(
      "nowrap",
    );
  });
});

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

/** GUI pass, 2026-09-25: with the tab bar overflowing (three tabs in a 520px panel), WebKitGTK's
 *  horizontal scrollbar sat over the lower 21px of the 41px bar and ate every click there -- half
 *  of each tab did not select it. The bar still scrolls (the active tab is scrolled into view, and
 *  `h`/`l` move); it just draws no scrollbar, as tmux's status line has none. */
it("draws no scrollbar on the tab bar, so a click anywhere on a tab reaches it", () => {
  const sheet = stripComments(css);
  const bar = rulesMatching(sheet, ".tab-bar").find((r) => r.selector.trim() === ".tab-bar");
  expect(bar?.body).toMatch(/overflow-x:\s*auto/);
  expect(bar?.body).toMatch(/scrollbar-width:\s*none/);
  const webkit = rulesMatching(sheet, ".tab-bar::-webkit-scrollbar");
  expect(webkit.map((r) => r.body).join(";")).toMatch(/display:\s*none/);
});

/** sw-theme-2, whole-branch review: `tokens.rs` derives `--nv-surface-fg` (and `--nv-surface-muted`)
 *  guarded against `--nv-surface`, but nothing read them, so every card still drew the body's own
 *  `--nv-fg`/`--nv-muted` -- guarded against `bg`, not against the fill under them. Under the
 *  shipped `zaibatsu` colorscheme (`NormalFloat` bg = Normal's fg) that is white text on a white
 *  card. This renders the panel's real surface-painted boxes, finds each text's nearest fill, and
 *  requires every text on a `--nv-surface` fill to be drawn in a surface-guarded colour (or a
 *  `--nv-syn-*` token, the deliberately unguarded syntax exemption). Declarations, not rendered
 *  colour: jsdom inherits `color` but never resolves `var()`, which is exactly what makes the token
 *  name comparable here. */
describe("text on a --nv-surface fill (sw-theme-2)", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  /** The nearest fill behind `el`: its own or an ancestor's `background` token. `null` for the body. */
  function nearestFill(el: Element | null): string | null {
    for (let node = el; node !== null; node = node.parentElement) {
      const style = getComputedStyle(node);
      for (const value of [style.background, style.backgroundColor]) {
        if (/var\(--nv-[a-z-]+\)/.test(value ?? "")) return value.match(/var\(--nv-[a-z-]+\)/)![0];
      }
    }
    return null;
  }

  function surfaceFixtures(): string {
    const call = (seq: number, name: string, input: unknown, gated = false) =>
      renderToStaticMarkup(
        createElement(
          "div",
          null,
          renderToolCall({ seq, toolUseId: `t${seq}`, name, input, result: { content: "ok", isError: false } }, true, {
            gated,
          }),
        ),
      );
    const card = (seq: number, toolName: string, input: unknown, extra: Record<string, unknown> = {}) =>
      renderToStaticMarkup(
        createElement(PermissionCard, {
          request: { seq, permissionId: `p${seq}`, toolUseId: null, toolName, input, ...extra },
          sessionEnded: extra.sessionEnded === true,
          onAnswer: () => {},
        }),
      );
    const rows = [
      call(1, "Bash", { command: "cargo test" }),
      call(2, "Read", { file_path: "/p/a.rs" }),
      call(3, "ToolSearch", { query: "x" }),
      call(4, "mcp__demo__lookup", { k: "v" }),
      call(5, "Bash", { command: "cargo test" }, true),
      call(6, "Edit", { file_path: "/p/a.rs", old_string: "a\n", new_string: "b\n" }),
      // `MessageList`'s collapsed run row (a component-internal element, drawn here as it renders).
      `<div class="tool-card tool-card-run">Bash ×2 · Read ×1</div>`,
      renderMarkdown("```\nplain code\n```"),
      renderMarkdown("```rust\nfn main() {}\n```"),
      card(7, "Bash", { command: "cargo test" }),
      card(8, "Edit", { file_path: "/p/a.rs", old_string: "a\n", new_string: "b\n" }),
      card(9, "Write", { file_path: "/p/b.rs", content: "x\n" }),
      card(10, "mcp__demo__lookup", { k: "v" }),
      card(11, "Write", { file_path: "/p/.git/config", content: "x\n" }, {
        providerPrompt: {
          reason: "writes inside .git",
          description: null,
          blockedPath: null,
          matchedAskRule: null,
          unrecognizedOrigin: null,
        },
      }),
      card(12, "Bash", { command: "ls" }, { sessionEnded: true }),
      `<div class="handoff-card"><pre class="handoff-command">claude --resume abc</pre></div>`,
    ];
    const whichKey = renderToStaticMarkup(
      createElement(WhichKeyBox, {
        title: "Space b",
        entries: [
          { key: "b", label: "other tab", group: false, disabled: false },
          { key: "d", label: "close tab", group: false, disabled: true },
          { key: "g", label: "+goto", group: true, disabled: false },
        ],
        onPick: () => {},
      }),
    );
    return (
      `<div class="agent-ui-root"><div class="agent-ui-scroller"><div class="message-list">` +
      rows.map((row) => `<div class="row"><span class="row-sign"></span><div class="row-body">${row}</div></div>`).join("") +
      `</div>${whichKey}</div></div>`
    );
  }

  it("draws every text on a surface fill in a colour Rust guards against that surface", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = surfaceFixtures();
    const offenders: string[] = [];
    let onSurface = 0;
    for (const el of Array.from(document.body.querySelectorAll("*"))) {
      const ownText = Array.from(el.childNodes)
        .filter((n) => n.nodeType === Node.TEXT_NODE)
        .map((n) => n.textContent ?? "")
        .join("")
        .trim();
      if (ownText === "" || nearestFill(el) !== "var(--nv-surface)") continue;
      onSurface += 1;
      const color = getComputedStyle(el).color;
      if (!/^var\(--nv-(surface-fg|surface-muted|syn-[a-z]+)\)$/.test(color)) {
        offenders.push(`<${el.tagName.toLowerCase()} class="${el.className}"> "${ownText.slice(0, 24)}": ${color || "(none)"}`);
      }
    }
    expect(onSurface, "the fixture must actually put text on a surface fill").toBeGreaterThan(15);
    expect(offenders).toEqual([]);
  });

  it("never draws the surface pair on any other fill", () => {
    // The other direction: `.permission-card-input` paints its own `bg` inside the card, and the
    // diff a tool call renders in the transcript sits on `bg` too. Text there must be the body's pair.
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = surfaceFixtures();
    const offenders: string[] = [];
    let offSurface = 0;
    for (const el of Array.from(document.body.querySelectorAll("*"))) {
      const ownText = Array.from(el.childNodes)
        .filter((n) => n.nodeType === Node.TEXT_NODE)
        .map((n) => n.textContent ?? "")
        .join("")
        .trim();
      if (ownText === "" || nearestFill(el) === "var(--nv-surface)") continue;
      offSurface += 1;
      const color = getComputedStyle(el).color;
      if (/surface/.test(color)) offenders.push(`<${el.tagName.toLowerCase()} class="${el.className}"> "${ownText.slice(0, 24)}": ${color}`);
    }
    expect(offSurface, "the fixture must put text on another fill too").toBeGreaterThan(3);
    expect(offenders).toEqual([]);
  });
});

describe("VISUAL mode (spec docs/superpowers/specs/2026-09-28-browse-visual-mode-design.md, D6/D14)", () => {
  it("index.css's user-select: none selectors equal visual.ts's own VISUAL_CHROME, exactly", () => {
    const rules = splitRules(stripComments(css));
    const found = new Set<string>();
    for (const rule of rules) {
      if (!/user-select\s*:\s*none\s*;?/.test(rule.declarations)) continue;
      for (const selector of rule.selector.split(",")) found.add(selector.trim());
    }
    expect(found).toEqual(new Set(VISUAL_CHROME));
  });

  it("hides every VISUAL_CHROME member under data-visual-copying (D8) -- fix round 1, the rule copySelectionText's attribute actually needed", () => {
    const rules = splitRules(stripComments(css));
    const found = new Set<string>();
    for (const rule of rules) {
      if (!/display\s*:\s*none\s*;?/.test(rule.declarations)) continue;
      for (const selector of rule.selector.split(",")) {
        const trimmed = selector.trim();
        const match = /^\[data-visual-copying\]\s+(.+)$/.exec(trimmed);
        if (match) found.add(match[1].trim());
      }
    }
    expect(found).toEqual(new Set(VISUAL_CHROME));
  });

  it("highlights a selection in nvim's Visual colour while VISUAL/V-LINE is on, and turns off the cursorline fill where the two coincide", () => {
    // Comments stripped first: `rulesMatching` is a naive substring matcher, and this file's own
    // doc comment on the rule below mentions ".row-current" in backticks -- without stripping,
    // that comment text (attached to the WRONG rule's captured selector) is what "found" it.
    const stripped = stripComments(css);
    const selectionRule = rulesMatching(stripped, "::selection").find((r) => r.selector.includes("[data-visual]"));
    expect(selectionRule, "no .message-list[data-visual] ::selection rule").toBeDefined();
    expect(selectionRule!.body).toContain("background: var(--nv-cursorline)");
    // Text keeps its own colour: this rule must not also set `color`.
    expect(selectionRule!.body).not.toMatch(/(?<!background-)color\s*:/);
    const rowCurrentOverride = rulesMatching(stripped, ".row-current").find((r) => r.selector.includes("[data-visual]"));
    expect(rowCurrentOverride, "no .message-list[data-visual] .row-current rule").toBeDefined();
    expect(rowCurrentOverride!.body).toContain("background: none");
  });
});

/* rc.4 item 6 (handoff 2026-09-30, `defect-inline-code-hyphen-break.png`): an inline code span broke between the
   two hyphens of `--json`, leaving "-" at the end of one line and "-json" at the start of the next. A browser may
   break a line after a hyphen-minus, and no `hyphens`/`word-break` value stops that; `white-space: nowrap` does,
   but lets a very long span run out of its row. An atomic inline does both: an `inline-block` no wider than the
   row is laid out whole (its shrink-to-fit width is its content's width, so nothing inside it ever wraps, and
   it moves to the next line as one piece when it does not fit on this one), and only one longer than the whole
   row is capped at `max-width: 100%` and broken, anywhere, by `overflow-wrap`. Fenced code is `pre code`, which
   the selector leaves alone. jsdom has no layout: this pins the rule and that it reaches the span the
   markdown renderer emits, and the rendering itself is a screen's (`shell/MANUAL_VERIFICATION.md`). */
describe("inline code never breaks inside a token (rc.4 item 6)", () => {
  const rule = () => {
    const found = splitRules(stripComments(css)).filter(
      (r) => r.selector.trim() === ".row-assistant code:not(pre code)" && /display\s*:/.test(r.declarations),
    );
    expect(found, "one rule gives inline code its atomic layout").toHaveLength(1);
    return found[0].declarations;
  };

  it("is an atomic inline no wider than its row, broken only when longer than the row", () => {
    const declarations = rule();
    expect(declarations).toMatch(/display\s*:\s*inline-block\s*;/);
    expect(declarations).toMatch(/max-width\s*:\s*100%\s*;/);
    expect(declarations).toMatch(/overflow-wrap\s*:\s*anywhere\s*;/);
  });

  it("never turns wrapping off, which would let a long span run out of the row", () => {
    expect(rule()).not.toMatch(/white-space\s*:\s*(nowrap|pre)\b/);
  });

  it("reaches the code the markdown renderer emits for an inline span, and not a fenced block's", () => {
    document.body.innerHTML = `<div class="row row-assistant">${renderMarkdown("I added a `--json` flag.\n\n```\nls --json\n```\n")}</div>`;
    const selector = ".row-assistant code:not(pre code)";
    const inline = Array.from(document.querySelectorAll("code")).filter((c) => c.parentElement?.tagName !== "PRE");
    const fenced = Array.from(document.querySelectorAll("pre code"));
    expect(inline.map((c) => c.textContent)).toEqual(["--json"]);
    expect(fenced).toHaveLength(1);
    expect(inline.every((c) => c.matches(selector))).toBe(true);
    expect(fenced.some((c) => c.matches(selector))).toBe(false);
  });
});

/* rc.4 item 7 (handoff 2026-09-30, `defect-diff-scrollbar-overlap.png`): at panel text 0.9 an expanded one-line
   edit diff's horizontal scrollbar was drawn over its last (`+`) line. The scrollbar is WebKitGTK's overlay one, so
   it takes no room of its own: the diff box (`.permission-card-diff`, the `pre` that scrolls sideways) keeps a strip
   of padding under its last line for it to sit in. `scrollbar-gutter` does not help, it reserves the block-axis
   scrollbar's room only. jsdom draws no scrollbar, so this pins the room, not the pixels. */
describe("an expanded diff keeps room under its last line for its scrollbar (rc.4 item 7)", () => {
  /** The effective bottom padding, in px, of `.permission-card-diff`: its `padding` shorthand and `padding-bottom`
   *  read in source order, as the cascade applies them within one rule. */
  function bottomPaddingPx(): number {
    const rules = splitRules(stripComments(css)).filter((r) => r.selector.trim() === ".permission-card-diff");
    expect(rules, "one rule styles the diff box").toHaveLength(1);
    let bottom = 0;
    for (const declaration of rules[0].declarations.split(";")) {
      const [name, ...rest] = declaration.split(":");
      const value = rest.join(":").trim();
      if (name.trim() === "padding") {
        const parts = value.split(/\s+/).map((p) => (p === "0" ? 0 : parseFloat(p)));
        bottom = parts.length === 1 ? parts[0] : parts.length === 2 ? parts[0] : parts[2];
      } else if (name.trim() === "padding-bottom") {
        bottom = value === "0" ? 0 : parseFloat(value);
      }
    }
    return bottom;
  }

  it("still scrolls sideways, and keeps at least 10px under its last line", () => {
    const rule = splitRules(stripComments(css)).find((r) => r.selector.trim() === ".permission-card-diff")!;
    expect(rule.declarations).toMatch(/overflow-x\s*:\s*auto\s*;/);
    expect(bottomPaddingPx()).toBeGreaterThanOrEqual(10);
  });
});

/* rc.4 item 8 (handoff 2026-09-30, `t10.png`, `t39-hero-alt.png`, `04-hint.png`): the permission card's reason box
   kept the browser's intrinsic width -- about twenty characters -- so its placeholder read "Reason (shown to the ag"
   -- and Approve/Deny kept WebKit's own 13.33px form-control font whatever the panel's text size was, so at a larger
   panel size the prose, the card's title and the reason box grew and the two buttons did not. The box now fills the
   card (an ellipsis rather than a hard clip where even that is too narrow), and the buttons take the panel's face and
   its `--fs-*` scale like every other control in it (the composer, the rename box). jsdom has no layout: this pins the
   rules, and the pixels are owed a screen. */
describe("the permission card's reason box and buttons follow the panel (rc.4 item 8)", () => {
  /** Every declaration block whose selector list names `selector` exactly, in source order. */
  const declarationsFor = (selector: string): string => {
    const blocks = splitRules(stripComments(css))
      .filter((r) => r.selector.split(",").some((s) => s.trim() === selector))
      .map((r) => r.declarations);
    expect(blocks.length, `${selector} has a rule`).toBeGreaterThan(0);
    return blocks.join("\n");
  };

  it("the reason box fills the card instead of its intrinsic width, and ends an over-long placeholder in an ellipsis", () => {
    const declarations = declarationsFor(".permission-card input");
    expect(declarations).toMatch(/width\s*:\s*100%\s*;/);
    expect(declarations).toMatch(/box-sizing\s*:\s*border-box\s*;/);
    expect(declarations).toMatch(/text-overflow\s*:\s*ellipsis\s*;/);
  });

  it("the reason box still takes the panel's face and scale", () => {
    const declarations = declarationsFor(".permission-card input");
    expect(declarations).toMatch(/font\s*:\s*inherit\s*;/);
    expect(declarations).toMatch(/font-size\s*:\s*var\(--fs-base\)\s*;/);
  });

  it("Approve, Deny and the third button take the panel's face and --fs-base, not WebKit's form-control font", () => {
    const declarations = declarationsFor(".permission-card-buttons button");
    expect(declarations).toMatch(/font\s*:\s*inherit\s*;/);
    expect(declarations).toMatch(/font-size\s*:\s*var\(--fs-base\)\s*;/);
  });
});

/* Fix round (Claude + Codex review of rc.4 item 6): the atomic layout above reaches every inline span under an
   assistant row, which broke two things it was never meant to touch. (a) A table cell's words stay whole
   (`.table-scroll td { overflow-wrap: normal }`, T1), but the span's own `overflow-wrap: anywhere` and
   `max-width: 100%` beat the cell's inherited value, so an identifier in a cell broke again and was capped to
   the cell's width. (b) An inline-block does not take its ancestors' text decoration into its own box (CSS
   Text Decoration 3: "not propagated to ... atomic inline-level descendants"), so the code in a link lost its
   underline and the code in `~~ ~~` its strike-through -- each takes it back on its own box, explicitly
   rather than by `inherit`, since a `<strong>` between the link and the span would have a computed `none`.
   `winningDeclarationOn` reads which rule wins; jsdom draws nothing. */
describe("inline code keeps its cell's words and its links' decoration (rc.4 item 6, fix round)", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });
  function mount(markdown: string): HTMLElement[] {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = `<div class="message-list"><div class="row row-assistant"><div class="row-body">${renderMarkdown(markdown)}</div></div></div>`;
    return Array.from(document.querySelectorAll<HTMLElement>("code")).filter((c) => c.parentElement?.tagName !== "PRE");
  }

  it("inside a table cell the span is not broken and not capped: overflow-wrap normal, max-width none", () => {
    const [code] = mount("| a |\n|---|\n| `supercalifragilistic-identifier --json` |");
    expect(code.closest(".table-scroll td")).not.toBeNull();
    expect(winningDeclarationOn(code, "overflow-wrap")).toBe("normal");
    expect(winningDeclarationOn(code, "max-width")).toBe("none");
  });

  it("outside a table the span is still capped at the row and broken only when longer than it", () => {
    const [code] = mount("a `--json` flag");
    expect(code.closest(".table-scroll")).toBeNull();
    expect(winningDeclarationOn(code, "overflow-wrap")).toBe("anywhere");
    expect(winningDeclarationOn(code, "max-width")).toBe("100%");
  });

  it("code inside a link is underlined on its own box, nested in a <strong> or not", () => {
    const [plain, nested] = mount("[`--json`](https://example.com/) and [**`--yaml`**](https://example.com/)");
    expect(plain.closest("a")).not.toBeNull();
    expect(nested.closest("strong")).not.toBeNull();
    expect(winningDeclarationOn(plain, "text-decoration")).toBe("underline");
    expect(winningDeclarationOn(nested, "text-decoration")).toBe("underline");
  });

  it("code inside ~~strike-through~~ is struck through on its own box", () => {
    const [code] = mount("~~a `--json` flag~~");
    expect(code.closest("del")).not.toBeNull();
    expect(winningDeclarationOn(code, "text-decoration")).toBe("line-through");
  });

  it("code inside a struck-through link carries both", () => {
    const [code] = mount("~~[`--json`](https://example.com/)~~");
    expect(code.closest("del")).not.toBeNull();
    expect(code.closest("a")).not.toBeNull();
    expect(winningDeclarationOn(code, "text-decoration")).toBe("underline line-through");
  });

  it("plain inline code carries no decoration of its own", () => {
    const [code] = mount("a `--json` flag");
    expect(winningDeclarationOn(code, "text-decoration")).toBeNull();
  });
});

describe("the start screen's stage", () => {
  it("is the trust question's containing block and a column like the root, so the band stays below the overlay", () => {
    const html = '<div class="agent-ui-root"><div class="empty-stage"><div class="trust-prompt"></div></div></div>';
    expect(winningDeclaration(html, ".empty-stage", "position")).toBe("relative");
    expect(winningDeclaration(html, ".empty-stage", "display")).toBe("flex");
    expect(winningDeclaration(html, ".empty-stage", "flex-direction")).toBe("column");
    // Absolute against the stage (not the root), covering all of it.
    expect(winningDeclaration(html, ".trust-prompt", "position")).toBe("absolute");
    expect(winningDeclaration(html, ".trust-prompt", "inset")).toBe("0px");
  });
});
