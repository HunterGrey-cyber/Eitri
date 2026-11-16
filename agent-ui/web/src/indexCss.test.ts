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
import { PASS_THROUGH_LAYERS } from "./lookOnScreen";
// The sideways guard renders the panel's real `<pre>`s.
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { renderToolCall } from "./toolRegistry";
import { renderMarkdown } from "./markdown";
import { PermissionCard } from "./components/PermissionCard";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { WhichKeyBox } from "./components/WhichKeyBox";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { VISUAL_CHROME } from "./visual";
import { Composer } from "./components/Composer";
import { Row } from "./components/Row";
import { THINKING_PREVIEW_LINES } from "./components/MessageList";
import type { ToolCallRecord } from "./types";

/** Strip CSS comments while preserving markers inside strings. A regex can treat two `content`
 *  values as a comment pair and delete real rules between them while leaving balanced braces,
 *  hiding those rules from every guard downstream. Quote awareness is needed here as well as
 *  in `splitRules`, before any rule is scanned. */
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
/** A branch inside the winbar, status line or status band, including their descendants.
 *  The band uses the editor's statusline fill (`--nv-chrome`) to continue it across the divider. */
const CHROME_BRANCH = /(?:^|[\s.])(?:winbar|status-line|status-band)\b/;
/** The solid cursor block: the current row's sign cell, a focused button and its children,
 *  the band's `↓N` button, the chooser's current sign cell, CARET's one-character selection,
 *  and a permission card's primary answer. Each uses the body's reversed pair. Nothing else. */
const CURSOR_BRANCH =
  /^(?:\.row-current \.row-sign|\.agent-ui-root button:focus(?: \*)?|\.band-unread|\.chooser-row\.current \.chooser-sign|\.message-list\[data-visual="caret"\] ::selection|\.permission-card-buttons button:first-child)$/;
/** A selection fill WebKit paints as given: one `--nv-*` token at 99% opacity. 99%, not 100%: WebKit
 *  turns an opaque `::selection` background into a translucent one meant for white pages. */
const SELECTION_FILL = /^background: color-mix\(in srgb, var\(--nv-[a-z-]+\) 99%, transparent\)$/;
/** The global `f` HINT's label, and nothing else. */
const HINT_BRANCH = /^\.hint-label$/;
/** The which-key box's own keycap glyph, and nothing else -- a
 *  selector branch ending in exactly `.wk-key`. */
const WK_KEY_BRANCH = /(?:^|[\s.])wk-key$/;
/** The band's `⏵⏵` mode glyph, and nothing else -- a
 *  selector branch that is `.mode-glyph` optionally followed by a `[data-mode-name="..."]`
 *  attribute selector, narrowed so it cannot widen to `.band-mode`/`.mode-pill`/anything else the
 *  glyph sits beside. */
const MODE_GLYPH_BRANCH = /(?:^|[\s.])mode-glyph(?:\[data-mode-name="[a-z]+"\])?$/;
/** The status band's mode block, drawn in the editor's own statusline colours: exactly
 *  `.band-mode`, optionally narrowed by `[data-mode="..."]`. Its text may use a `--nv-status-*-fg`
 *  only where the same rule paints that mode's `--nv-status-*-bg`. Like syntax colours, this pair
 *  is deliberately unguarded because it is the editor's own colour. */
const BAND_MODE_BRANCH = /^\.band-mode(?:\[data-mode="[a-z]+"\])?$/;
/** State glyphs, each with exactly one signal colour. Their non-text threshold is 3:1
 *  (WCAG 1.4.11): `--nv-ok`/`--nv-error` are guarded against `bg`, and `--nv-chrome-accent`
 *  against both `chrome` and `bg` so it stays visible on either. The numbers beside edit signs
 *  keep the head's ink; a diff line keeps its own text ink beside its coloured `+`/`-`. */
const GLYPH_COLOURS: Record<string, string> = {
  ".activity-glyph": "--nv-chrome-accent",
  '.row-tool[data-sign="◐"]::before': "--nv-chrome-accent",
  '.row-tool[data-sign="✗"]::before': "--nv-error",
  ".tool-failure-words": "--nv-error",
  ".edit-count-added .edit-sign": "--nv-ok",
  ".edit-count-removed .edit-sign": "--nv-error",
  ".diff-added .diff-gutter": "--nv-ok",
  ".diff-removed .diff-gutter": "--nv-error",
  ".permission-card .diff-added .diff-gutter": "--nv-ok",
  ".permission-card .diff-removed .diff-gutter": "--nv-error",
};
/** Branches allowed to declare the surface pair: which-key, edit heads, handoff commands,
 *  fenced code, tool output and table heads. Matching a name does not establish its actual fill;
 *  the rendered-element checks below verify the nearest fill behind the text. */
const SURFACE_BRANCH =
  /(?:^|[\s.])(?:which-key-box|tool-card(?:-[a-z]+)?|permission-card(?:-[a-z-]+)?|handoff-command|code-block|tool-result-(?:preview|body)|table-scroll th)\b/;
/** The three tinted fills, each with the one pair `tokens.rs` guards against
 *  it, and the selector branches that sit on it: your own tint (`--nv-tint`: your message and an
 *  inline code span in a reply), a permission card's warn tint, and a failure's
 *  error tint (the lost-session card in the foot). As with `SURFACE_BRANCH`, where
 *  the text really lands is proven by rendering ("text on a tinted fill", at the bottom of this file);
 *  this keeps each pair from being declared anywhere else. */
const TINT_BRANCHES: { pair: RegExp; branch: RegExp }[] = [
  {
    pair: /^color: var\(--nv-tint-(fg|muted)\);$/,
    branch: /(?:^|[\s.])row-prompt\b|^\.row-assistant code:not\(pre code\)$/,
  },
  { pair: /^color: var\(--nv-warn-tint-(fg|muted)\);$/, branch: /(?:^|[\s.])permission-card\b/ },
  {
    pair: /^color: var\(--nv-error-tint-(fg|muted)\);$/,
    branch: /^\.agent-ui-conversation > \.row-error\b/,
  },
];

/**
 * The rule this file actually enforces, in one sentence: **a colour may be used as text only where
 * Rust guards it for text against the surface it sits on.** `tokens.rs` guards `fg`/`muted` for
 * text against `bg` (`TEXT_CONTRAST`, 4.5:1) -- the panel body's surface -- and separately guards
 * `chrome-fg`/`chrome-muted` for text against `chrome` (also 4.5:1, `tokens.rs:209-210`) -- the
 * winbar/status-line's surface, painted in nvim's own `StatusLine` colours rather than the body's.
 * Neither pair is interchangeable with the other: `chrome`-guarded text on the plain body, or
 * `bg`-guarded text on chrome, was never measured against the surface actually under it. The one
 * other exemption is `--nv-syn-*` inside a `hljs-*` rule, deliberately
 * UNguarded, because syntax tokens are meant to be the editor's own colours; see the big comment on
 * the real guard test below for why that one is not a contrast exemption at all. The same
 * non-guarantee applies more narrowly to `--nv-syn-keyword` on exactly
 * the which-key box's own `.wk-key` glyph.
 *
 * Returns every `color:` declaration in `source` that none of the pairings this function admits
 * allows: the three above, and the surface, tint, cursor, hint, keycap, mode-glyph, band-mode and
 * glyph pairs its body names. Shared by the real guard test and the tests proving none of the
 * exemptions can quietly widen -- on purpose. Before this was hoisted, each test carried its own copy of this
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
    // CARET's block is `--nv-fg` at 99% opacity (see `SELECTION_FILL`): the same fill to the eye, so the
    // same text guard holds on it.
    const onCursorSurface =
      everyBranchIsOnSurface(selector, CURSOR_BRANCH) &&
      /(?:^|;)\s*background: (?:var\(--nv-fg\)|color-mix\(in srgb, var\(--nv-fg\) 99%, transparent\));/.test(body);
    const onHintSurface =
      everyBranchIsOnSurface(selector, HINT_BRANCH) && /(?:^|;)\s*background: var\(--nv-hint-bg\);/.test(body);
    const onWkKeySurface = everyBranchIsOnSurface(selector, WK_KEY_BRANCH);
    const onModeGlyphSurface = everyBranchIsOnSurface(selector, MODE_GLYPH_BRANCH);
    const statusFill = everyBranchIsOnSurface(selector, BAND_MODE_BRANCH)
      ? (/(?:^|;)\s*background: var\(--nv-status-(normal|insert|visual)-bg\);/.exec(body)?.[1] ?? null)
      : null;
    const branches = selector.split(",").map((b) => b.trim());
    const glyphColour = branches.every((b) => GLYPH_COLOURS[b] !== undefined && GLYPH_COLOURS[b] === GLYPH_COLOURS[branches[0]])
      ? GLYPH_COLOURS[branches[0]]
      : null;
    const onCardSurface = everyBranchIsOnSurface(selector, SURFACE_BRANCH);
    const tintPairs = TINT_BRANCHES.filter(({ branch }) => everyBranchIsOnSurface(selector, branch)).map(({ pair }) => pair);
    for (const declaration of body.match(/(?<![a-z-])color:[^;]*;/g) ?? []) {
      if (onHljsSurface && /^color: var\(--nv-syn-[a-z]+\);$/.test(declaration)) continue;
      if (onChromeSurface && /^color: var\(--nv-chrome-(fg|muted)\);$/.test(declaration)) continue;
      // Text on a card uses the surface pair, guarded against `surface` in Rust,
      // and only inside the boxes that paint it.
      if (onCardSurface && /^color: var\(--nv-surface-(fg|muted)\);$/.test(declaration)) continue;
      // Text on a tint: that tint's own pair, guarded against it in Rust, and only on its own branches.
      if (tintPairs.some((pair) => pair.test(declaration))) continue;
      // The which-key box's own keycap: the same deliberately-
      // unguarded exemption `--nv-syn-*` gets inside `hljs-*` above, extended to the one place
      // outside a code block that also wants a "this is a key" glyph -- narrowed to exactly
      // `.wk-key`, so it cannot widen to `.wk-group`/`.wk-disabled`/anything else in the box.
      if (onWkKeySurface && /^color: var\(--nv-syn-keyword\);$/.test(declaration)) continue;
      // The band's own mode glyph: `--nv-warn`/`--nv-error`
      // as a glyph colour, not text -- `--nv-warn`/`--nv-error` are guarded at `UI_CONTRAST` (3.0)
      // against `bg`, not the 4.5:1 text needs, the same reasoning every other signal colour in
      // this file gets kept off text for.
      if (onModeGlyphSurface && /^color: var\(--nv-(warn|error)\);$/.test(declaration)) continue;
      // The band's mode block: the statusline mode's own fg, only on the same mode's fill.
      if (statusFill !== null && declaration === `color: var(--nv-status-${statusFill}-fg);`) continue;
      // A state glyph in its own one colour (`GLYPH_COLOURS`), and only that one.
      if (glyphColour !== null && declaration === `color: var(${glyphColour});`) continue;
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

/** Top-level `:root` custom properties, mapped to raw declared-value text. jsdom never
 *  substitutes `var()`, so `expandVars` resolves these from the stylesheet itself. A flat regex
 *  suffices because the no-nesting assertion pins that premise. The dotAll flag is essential:
 *  a value such as `calc(\n 100cqw\n)` spans lines and must not disappear from the map. */
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
 *  matching the fraction as text. The text scale is one number and seven ratios of
 *  it. A string match on `"* 12 / 14"` cannot tell a correct ratio from a wrong one written with
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

/** Custom properties declared on exactly `.row`, read from source so a test can require none. */
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

/** One CSS block: its prelude, its OWN declarations (a nested block's text belongs to that block,
 *  not to this one), and the selector of the block it sits inside (`null` at the top level). */
type CssRule = { selector: string; declarations: string; parent: string | null };

/** Split blocks with brace-depth and quote awareness. Nested blocks are emitted separately;
 *  the parent's declarations end at the last `;` before a child's prelude, so neither can hide
 *  the other. Braces and semicolons in strings are literal. Unbalanced braces throw rather than
 *  yielding truncated blocks. Cascade checks and custom-property scans share this parser so
 *  they cannot drift apart. */
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

/** Motion declarations: animation and transition shorthands, longhands and vendor prefixes.
 *  Search declarations only: selector names can contain the same words without causing motion.
 *  The lookbehind rejects names such as `foo-animation`; matching is case-insensitive because
 *  CSS property names are. Escaped identifiers need a real tokenizer and remain outside this
 *  guard's coverage, as the control below records. */
const MOTION_PROPERTY = /(?<![a-zA-Z-])(?:-webkit-|-moz-|-ms-|-o-)?(?:animation|transition)(?:-[a-z-]+)?\s*:/i;
function movesSomething(rule: CssRule): boolean {
  return MOTION_PROPERTY.test(rule.declarations);
}

/** Custom-property declarations at any nesting depth. Unlike `ROOT_TOKENS`, this walks parsed
 *  blocks, so a declaration under `@media` or on `:root:not(...)` cannot evade the scan through
 *  selector spelling. DotAll keeps multiline values visible, including container-query units. */
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

/** Style rules in document order, including those inside `@media`, `@supports` and `@layer`.
 *  Flatten grouping rules unconditionally: a forbidden shape at only some widths still counts.
 *  Asking jsdom to evaluate conditions would hide those rules as the sheet grows more complex. */
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
 *  implementation of CSS specificity for `:not()`'s own argument, or of `:is()`/`:where()`,
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

/** Longhands for the shorthands these checks inspect. Reading only a shorthand misses a later
 *  longhand override, such as `border-left-style: none` cancelling a border. Extend this table
 *  only when a new shorthand check needs it; it is not exhaustive. */
const SHORTHAND_LONGHANDS: Record<string, string[]> = {
  "border-left": ["border-left-width", "border-left-style", "border-left-color"],
};

/** Cascade priority: importance, then specificity, then source order at a tie. An `!important`
 *  declaration beats a plain one regardless of either declaration's specificity or order. */
function beatsCurrentWinner(
  candidate: { important: boolean; spec: [number, number, number]; order: number },
  current: { important: boolean; spec: [number, number, number]; order: number },
): boolean {
  if (candidate.important !== current.important) return candidate.important;
  const cmp = compareSpecificity(candidate.spec, current.spec);
  if (cmp !== 0) return cmp > 0;
  return candidate.order >= current.order;
}

/** Read the winning raw declaration through `Element.matches` over parsed CSSOM rules.
 *  Importance precedes specificity and source order, so even an earlier, less-specific
 *  `!important` rule must win over a plain one.
 *
 *  Known shorthand longhands compete in the same race. If a longhand wins, return `null`:
 *  reconstructing the resulting shorthand is not generally possible. Callers must assert the
 *  expected value, since merely accepting non-null can mistake an incomplete check for a win.
 *
 *  jsdom drops shorthands containing `var()` from computed styles: mounting a variable border
 *  and mounting no border yield identical computed longhands. A CSSOM rule's own `.style`
 *  retains the raw text, so read it from the rule the cascade selects. */
function winningDeclaration(html: string, elementSelector: string, property: string, extraCss = ""): string | null {
  document.head.innerHTML = `<style>${css}${extraCss}</style>`;
  document.body.innerHTML = html;
  const el = document.body.querySelector(elementSelector);
  if (el === null) throw new Error(`no element matches ${elementSelector}`);
  return winningDeclarationOn(el, property);
}

/** The same cascade for an element already mounted in the document, so checks can inspect
 *  every element of a kind rather than only the first selector match. */
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

  /** `⏵⏵` (U+23F5) is not in FiraCode Nerd Font, so
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
    // Signal colours have a 3:1 non-text guard or no guard, not the 4.5:1 text needs. On rose-pine
    // dawn, ungated 12px text measures 2.05:1 on a resume row's hover band to 3.84:1 on the lost-session
    // banner, versus about 12:1 before the theme pipeline. Keep signal colours beside text.
    // The lookbehind excludes `background-color:` and `border-color:` from text declarations.
    // Body and chrome text each need the pair guarded against their own fill; a reversed
    // StatusLine can make chrome Normal's fg, so a bg guard says nothing about text on chrome.
    // Syntax tokens deliberately keep the editor's colours, even when a scheme's comments are
    // unreadable. Guarding them would make code disagree with the buffer. Every selector branch
    // must earn its own exemption; one matching branch cannot exempt a whole group.
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

  it("admits each tint's pair as text only on that tint's own branches, every branch of them", () => {
    expect(unguardedTextColorDeclarations(".row-prompt .row-body { color: var(--nv-tint-fg); }")).toEqual([]);
    // The composer is off the tint, so the tint's pair is refused there.
    expect(unguardedTextColorDeclarations(".composer-browse-hint { color: var(--nv-tint-muted); }")).toEqual(["color: var(--nv-tint-muted);"]);
    expect(unguardedTextColorDeclarations(".permission-card-description { color: var(--nv-warn-tint-muted); }")).toEqual([]);
    // A failed result has no tint under it, so the tint's pair is refused there.
    expect(unguardedTextColorDeclarations(".tool-result .tool-result-truncated { color: var(--nv-error-tint-muted); }")).toEqual([
      "color: var(--nv-error-tint-muted);",
    ]);
    expect(
      unguardedTextColorDeclarations(".agent-ui-conversation > .row-error .row-hint { color: var(--nv-error-tint-muted); }"),
    ).toEqual([]);
    // Off its tint, each pair is refused: the body, another tint, a group with one branch elsewhere.
    expect(unguardedTextColorDeclarations(".turn-time { color: var(--nv-tint-muted); }")).toEqual(["color: var(--nv-tint-muted);"]);
    expect(unguardedTextColorDeclarations(".composer-hint { color: var(--nv-warn-tint-muted); }")).toEqual([
      "color: var(--nv-warn-tint-muted);",
    ]);
    expect(unguardedTextColorDeclarations(".permission-card-stale { color: var(--nv-error-tint-fg); }")).toEqual([
      "color: var(--nv-error-tint-fg);",
    ]);
    expect(unguardedTextColorDeclarations(".row-error pre { color: var(--nv-error-tint-fg); }")).toEqual([
      "color: var(--nv-error-tint-fg);",
    ]);
    expect(unguardedTextColorDeclarations(".composer-hint, .turn-time { color: var(--nv-tint-muted); }")).toEqual([
      "color: var(--nv-tint-muted);",
    ]);
    // And a tint is not a licence for a signal colour.
    expect(unguardedTextColorDeclarations(".permission-card { color: var(--nv-warn); }")).toEqual(["color: var(--nv-warn);"]);
  });

  it("still refuses a signal colour as text, even inside an hljs-* rule", () => {
    // Only syntax tokens are exempt inside highlight rules. Calling the real guard here means
    // widening its exemption also fails this control; independent copies could drift together.
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

  it("admits a state glyph's own signal colour on exactly that glyph, and nothing wider", () => {
    expect(unguardedTextColorDeclarations('.row-tool[data-sign="✗"]::before { color: var(--nv-error); }')).toEqual([]);
    expect(unguardedTextColorDeclarations(".edit-count-added .edit-sign { color: var(--nv-ok); }")).toEqual([]);
    expect(unguardedTextColorDeclarations(".activity-glyph { color: var(--nv-chrome-accent); }")).toEqual([]);
    // The wrong colour on a glyph, the number beside a sign, a whole count, and a group with one branch
    // that is not a glyph are all refused.
    expect(unguardedTextColorDeclarations('.row-tool[data-sign="✗"]::before { color: var(--nv-ok); }')).toEqual([
      "color: var(--nv-ok);",
    ]);
    expect(unguardedTextColorDeclarations(".edit-count-added { color: var(--nv-ok); }")).toEqual(["color: var(--nv-ok);"]);
    expect(unguardedTextColorDeclarations(".activity-line { color: var(--nv-chrome-accent); }")).toEqual([
      "color: var(--nv-chrome-accent);",
    ]);
    expect(unguardedTextColorDeclarations(".edit-count-added .edit-sign, .turn-changes { color: var(--nv-ok); }")).toEqual([
      "color: var(--nv-ok);",
    ]);
  });

  it("admits --nv-bg on a permission card's primary button only where the same rule fills it with --nv-fg", () => {
    expect(
      unguardedTextColorDeclarations(".permission-card-buttons button:first-child { background: var(--nv-fg); color: var(--nv-bg); }"),
    ).toEqual([]);
    expect(unguardedTextColorDeclarations(".permission-card-buttons button:first-child { color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
    expect(unguardedTextColorDeclarations(".permission-card-buttons button { background: var(--nv-fg); color: var(--nv-bg); }")).toEqual([
      "color: var(--nv-bg);",
    ]);
  });

  it("still refuses --nv-chrome-accent as text inside the winbar", () => {
    // `chrome_accent` has only the 3:1 non-text guard. Chrome text needs the 4.5:1 guard held by
    // chrome-fg/chrome-muted, so an accent on winbar text must still fail. A matching tier in a
    // doc comment is no substitute for this check.
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
    // Session rows open unselected on "New session". Opacity lowers fg contrast at 0.7 to 3.34:1
    // on rose-pine dawn and 2.93:1 on its hover band. The selected row uses a fill instead.
    // Include both `row-choice` and `session-choice`, since shared grid rules can dim either.
    const rules = withoutComments.match(/[^{}]*(session-choice|row-choice)[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("does not dim the activity line or the band with opacity either", () => {
    // Opacity also lowers already-guarded muted text below its guaranteed contrast.
    const rules = withoutComments.match(/\.(?:status-band|activity-line)[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("the status band is one editor row high, and never grows", () => {
    // The band must align with the editor's statusline. A min-height grows with children's font
    // metrics, especially the symbol fallback; fixed `height` plus `overflow: hidden` stops that.
    // `--nv-editor-row` carries `ThemeTokens.editor_row_px`, the editor's own cell height.
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
    // The composer is the foot's boundary. No top rule subtracts from the band's fixed height,
    // so its line box shares the editor statusline's centre line.
    expect(decl("border-top")).toBeUndefined();
    expect(decl("line-height")).toBe("var(--nv-editor-row, 24px)");
  });

  /* The in-flight indicator is the only motion; absence must mean no animation anywhere in the
     stylesheet. Negative controls use the same splitter and declaration matcher as the real scan:
     (a) animation longhands on another selector still cause motion;
     (b) colour transitions spend part of their period below the full-strength contrast guard;
     (c) a final declaration without `;` must still be checked;
     (d) nested blocks must not hide their parent's declarations;
     (e) the same holds inside `@media`;
     (f) braces in strings are literal, not block boundaries.
     `splitRules` tracks brace depth and quotes. The separate no-nesting assertion also protects
     narrower regex scans whose rule-body patterns cannot span nested braces. */

  /** The selectors in a sheet that move something -- the whole guard in one call, so the negative
   *  controls exercise the real splitter rather than a hand-sliced approximation of it. */
  function movingSelectorsIn(sheet: string): string[] {
    // Strip here so controls exercise the entire scan path. Pre-stripped controls would miss a
      // stripper that hides real rules between comment markers inside strings.
    return splitRules(stripComments(sheet))
      .filter(movesSomething)
      .map((rule) => rule.selector);
  }

  /** Every keyframe declaration, including a final one without `;`: split at semicolons or
   *  the end of its own block so the final property cannot escape the guard. */
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

  /* WebKitGTK 2.52.6 resets the list's scrollTop to a stale value on elapsed-counter ticks
     when `.row` is a size query container, stopping follow about a second into a streaming reply.
     Moving it to `.row-body`, `.message-list`, or assistant rows alone also breaks follow.
     This source check guards the measured cause without a display; the engine check is
     `shell/tests/panel_stream_scroll.rs`.
     Only this stylesheet is read, so component inline styles can bypass it. `contain` and
     `content-visibility` were not measured alone and are not rejected here. In the engine check,
     a reader parked inside the reply detects a container; follow can re-snap before paint. */
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

  it("keeps all motion on the activity glyph, with one animation for each look", () => {
    // Only the working glyph moves; the tab chooses its animation.
    const moving = movingSelectorsIn(css);
    expect(moving.length).toBeGreaterThan(0);
    for (const selector of moving.flatMap((group) => group.split(",").map((s) => s.trim()))) {
      expect([
        ".activity-glyph[data-moving]",
        '.activity-glyph[data-moving][data-look="codex"]',
        '.activity-glyph[data-moving][data-look="acp"]',
      ]).toContain(selector);
    }
    const keyframeBlocks = withoutComments.match(/@keyframes\s+[\w-]+\s*\{/g) ?? [];
    expect(keyframeBlocks).toHaveLength(2);
    expect(keyframeBlocks[0]).toContain("turn-breath");
    expect(keyframeBlocks[1]).toContain("turn-pulse");

    // Negative control: a second animated selector and a second @keyframes.
    const twoAnimations =
      ".turn-activity .meter-fill { animation: turn-meter 1s; } .row-current { animation: pulse 1s; } " +
      "@keyframes turn-meter { from { transform: scaleX(0.25); } to { transform: scaleX(1); } } " +
      "@keyframes pulse { from { opacity: 0; } to { opacity: 1; } }";
    expect(movingSelectorsIn(twoAnimations)).toContain(".row-current");
    expect((twoAnimations.match(/@keyframes\s+[\w-]+\s*\{/g) ?? []).length).toBeGreaterThan(1);

    // Longhands can animate a second element using the permitted keyframes, without another
    // keyframes block or an animation shorthand.
    const longhands = ".row-current .row-sign { animation-name: turn-meter; animation-duration: 2s; }";
    expect(movingSelectorsIn(longhands)).toEqual([".row-current .row-sign"]);
    expect(/(?<![a-z-])animation:/.test(longhands)).toBe(false);

    // A 300ms colour transition spends part of its period below the contrast floor Rust guards
    // only at full strength.
    expect(movingSelectorsIn(".tool-card { transition: background-color 300ms linear; }")).toEqual([".tool-card"]);
    expect(movingSelectorsIn(".tool-card:hover { -webkit-transition: color 1s; }")).toEqual([".tool-card:hover"]);
    // ...and it is a DECLARATION guard, not a text search: a class whose NAME contains the word is
    // not a motion, and must not be reported as one.
    expect(movingSelectorsIn(".transition-demo:hover { color: var(--nv-fg); }")).toEqual([]);

    // A nested block must not hide its parent's motion: the parent moves, the child does not.
    expect(movingSelectorsIn(".tool-card { transition: background-color 300ms linear; &:hover { color: var(--nv-fg); } }")).toEqual([
      ".tool-card",
    ]);
    // Negative control (e), the same shape one level down, inside an @media.
    expect(
      movingSelectorsIn("@media (min-width: 1px) { .tool-card { transition: opacity 300ms; &:hover { color: var(--nv-fg); } } }"),
    ).toEqual([".tool-card"]);
    // A brace inside a string ends no block; the animation after it is still this rule's declaration.
    expect(movingSelectorsIn('.row-sign::before { content: "}"; animation: turn-meter 2s; }')).toEqual([
      ".row-sign::before",
    ]);
    // CSS property names are ASCII case-insensitive, so uppercase motion declarations must be seen.
    expect(movingSelectorsIn(".tool-card { TRANSITION: background-color 300ms linear; }")).toEqual([
      ".tool-card",
    ]);
    // And the limit: an ESCAPED ident is live
    // CSS too (`animati\\6fn` decodes to `animation`) and this guard does NOT catch it, because
    // decoding idents needs a real tokenizer. Asserted as it behaves, so the day someone adds one
    // this line is what tells them the guard was never claiming to.
    expect(movingSelectorsIn(".row-current { animati\\6fn: turn-meter 2s; }")).toEqual([]);
  });

  it("pulses Codex and ACP bullets by opacity only and rests both under reduced motion", () => {
    const frames = withoutComments.match(/@keyframes turn-pulse \{([\s\S]*?)\n\}/);
    expect(frames).not.toBeNull();
    const declarations = keyframeDeclarations(frames![1]);
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) expect(declaration).toMatch(/^opacity\s*:/);
    expect(frames![1]).toMatch(/0%,\s*100%\s*\{\s*opacity: 1;/);
    expect(frames![1]).toMatch(/50%\s*\{\s*opacity: 0\.5;/);
    const reduced = withoutComments.match(/@media \(prefers-reduced-motion: reduce\) \{[\s\S]*?\n\}/)![0];
    for (const look of ["codex", "acp"]) {
      const selector = `.activity-glyph[data-moving][data-look="${look}"]`;
      const rule = splitRules(withoutComments).find((r) =>
        r.selector.split(",").map((s) => s.trim()).includes(selector) && r.declarations.includes("turn-pulse"));
      expect(rule?.declarations).toContain("animation: turn-pulse calc(var(--meter-step, 300ms) * 4) ease-in-out infinite;");
      expect(reduced).toContain(selector);
    }
    expect(reduced).toContain("animation: none;");
    expect(reduced).toContain("opacity: 1;");
  });

  /// A quote-unaware stripper can treat two content values as a comment pair and delete real rules
    /// between them. Balanced braces cannot detect that loss; the scan must preserve those rules.
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

  it("rests the star at its full size under reduced motion, and breathes in exactly four steps otherwise", () => {
    // The star's cycle has two halves, each `steps(2, jump-end)`: full, halfway down, bottom,
    // halfway back up. One visible state per `--meter-step`, never eased: an eased pulse repaints
    // every frame, which typing cadence exists to prevent.
    const glyph = withoutComments.match(/\.activity-glyph\[data-moving\] \{[^}]*\}/);
    expect(glyph).not.toBeNull();
    expect(glyph![0]).toMatch(/animation: turn-breath calc\(var\(--meter-step, 300ms\) \* 4\) steps\(2, jump-end\) infinite;/);
    const frames = withoutComments.match(/@keyframes turn-breath \{([\s\S]*?)\n\}/);
    expect(frames).not.toBeNull();
    const rest = frames![1].match(/0%,\s*100% \{\s*transform: (scale\(([\d.]+)\));/);
    const mid = frames![1].match(/50% \{\s*transform: scale\(([\d.]+)\);/);
    expect(rest).not.toBeNull();
    expect(mid).not.toBeNull();
    const [full, low] = [Number(rest![2]), Number(mid![1])];
    const states = [full, full + (low - full) / 2, low, low + (full - low) / 2];
    for (const [got, want] of states.map((v, i) => [v, [1, 0.8, 0.6, 0.8][i]])) expect(got).toBeCloseTo(want, 10);

    // Reduced motion: no animation, and the star rests at the cycle's first state -- derived from the
    // keyframes rather than spelled twice.
    const reduced = withoutComments.match(/@media \(prefers-reduced-motion: reduce\) \{[\s\S]*?\n\}/);
    expect(reduced).not.toBeNull();
    expect(reduced![0]).toContain(".activity-glyph[data-moving]");
    expect(reduced![0]).toContain("animation: none;");
    expect(reduced![0]).toContain(`transform: ${rest![1]};`);
    // Negative control: `animation: none` on its own carries no resting transform.
    const bare = "@media (prefers-reduced-motion: reduce) {\n  .activity-glyph[data-moving] {\n    animation: none;\n  }\n}";
    expect(bare).not.toContain("transform:");
  });

  it("lets the typing cadence slow the star through --meter-step, never pause it, and keeps Rust's threshold equal to its step", () => {
    const glyph = withoutComments.match(/\.activity-glyph\[data-moving\] \{[^}]*\}/);
    expect(glyph).not.toBeNull();
    const shorthand = glyph![0].match(
      /animation: turn-breath calc\(var\(--meter-step, (\d+)ms\) \* (\d+)\) steps\((\d+), jump-end\) infinite;/,
    );
    expect(shorthand).not.toBeNull();
    const [, fallbackMs, factor, stepsPerHalf] = shorthand!;
    // One visible state lasts `--meter-step`: the duration is that times the number of states (two
    // halves of the cycle, `stepsPerHalf` each), so a step of 500ms really is 500ms between repaints.
    const halves = (withoutComments.match(/@keyframes turn-breath \{[\s\S]*?\n\}/)![0].match(/\d+%[^{]*\{/g) ?? []).length;
    expect(halves).toBe(2);
    const steps = String(Number(stepsPerHalf) * halves);
    expect(Number(factor)).toBe(Number(steps));
    // With no typing cadence set, the four-state meter repaints once per 300ms, or once per state.
    expect(Number(fallbackMs)).toBe(NATURAL_METER_STEP_MS);
    expect(Number(fallbackMs) * Number(steps)).toBe(1200);
    // The same number, in Rust, decides whether the page is told about typing at all
    // (`panel_cadence::SELF_DRIVEN_STEP_MS`): a cadence at least that slow is the only one the meter
    // could outpace. If the CSS step moves and this does not, the page is silently not told.
    const rust = panelCadenceRs.match(/pub const SELF_DRIVEN_STEP_MS: u32 = (\d+);/);
    expect(rust, "core/src/panel_cadence.rs names SELF_DRIVEN_STEP_MS").not.toBeNull();
    expect(Number(rust![1])).toBe(NATURAL_METER_STEP_MS);
    // The stream must slow without pausing: no rule anywhere in
    // the sheet pauses an animation, and the typing attribute has no rule of its own that could.
    expect(withoutComments).not.toMatch(/animation-play-state/i);
    expect(withoutComments).not.toMatch(/data-editor-typing/);
    // Negative control: a pause would be seen by the same check.
    expect(".activity-glyph[data-moving] { animation-play-state: paused; }").toMatch(/animation-play-state/i);
  });

  it("keeps the star's keyframes to transform only -- geometry cannot break the contrast guarantee, colour and alpha can", () => {
    const match = withoutComments.match(/@keyframes turn-breath \{([\s\S]*?)\n\}/);
    expect(match).not.toBeNull();
    const declarations = keyframeDeclarations(match![1]);
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^transform\s*:/);
    }

    // Negative control: an opacity fade with both declarations semicolon-terminated.
    const fade = "\n  from { transform: scaleX(0.25); opacity: 0.4; }\n  to { transform: scaleX(1); opacity: 1; }\n";
    expect(keyframeDeclarations(fade).some((d) => !d.startsWith("transform:"))).toBe(true);

    // A final declaration needs no trailing semicolon. The second assertion demonstrates why
    // a semicolon-only matcher misses this input.
    const unterminated = "\n  from { transform: scaleX(0.25); }\n  to { transform: scaleX(1); opacity: 1 }\n";
    expect(keyframeDeclarations(unterminated).filter((d) => !d.startsWith("transform:"))).toEqual(["opacity: 1"]);
    expect((unterminated.match(/[a-z-]+:[^;]*;/g) ?? []).filter((d) => !d.startsWith("transform:"))).toEqual([]);
  });

  /** A label cut with `…` followed by the working marker read `……`. */
  it("marks a working tab with a separated glyph that is not an ellipsis", () => {
    const rules = splitRules(withoutComments).filter((r) => r.selector.trim() === ".tab-working::after");
    expect(rules).toHaveLength(1);
    expect(rules[0].declarations).toMatch(/content:\s*" •"/);
  });
});

/**
 * Which declaration WINS, not merely which ones exist.
 *
 * A regex over rule TEXT cannot see specificity, and specificity silently defeats a planned rule:
 * a `.row-assistant pre` rule (0,1,1) would outweigh `.code-block` (0,1,0) and win the code block's
 * background back to `--nv-bg`; a boxed `.mode-selector button` rule (0,1,1) would outweigh
 * `.row`/`.row-choice` (0,1,0) and put `display: block`/`padding: 10px` back over the sign-column
 * grid; and `.mode-selector .session-choice button.selected` would outweigh `.row-choice.selected`.
 * Prose is memory, not a guard.
 *
 * So: attach the real stylesheet to a real DOM and read `getComputedStyle`. jsdom resolves class
 * and descendant selectors and applies specificity for ordinary (non-`var()`) declarations. Each
 * case below has its own test and its own negative control that adds the competing rule and shows
 * that it would win.
 *
 * THREE THINGS IT CANNOT DO, said plainly so nobody reads more into a green run than is there:
 *   - jsdom does not resolve `var()`. A winning `background: var(--nv-surface)` computes to the
 *     literal string "var(--nv-surface)". That is exactly what makes an assertion on such a
 *     property useful when nothing else competes for it -- the string names the token, so a
 *     different winner reads as a different string -- but it is a comparison of declarations, not
 *     of rendered colour.
 *   - jsdom's CSS engine fails to parse a SHORTHAND containing `var()`, so `border-left: 3px solid
 *     var(--nv-mode-browse)` sets no longhand at all here, and neither does `border: 1px solid
 *     var(--nv-border)`. Only a longhand such as `border-left-color` can be asserted.
 *   - **A higher-specificity `var()` value losing to a lower-specificity plain-literal value is
 *     decided by source order here, not by specificity.** When BOTH competing values are `var()`,
 *     or BOTH are plain literals, this engine picks the correct higher-specificity winner regardless
 *     of order (matches real cascade behaviour), and so it does when the literal is the
 *     higher-specificity side. Only higher-specificity-`var()` vs. lower-specificity-literal is
 *     broken: the literal wins whenever it is LAST in the stylesheet, the `var()` side whenever IT
 *     is last. A test on such a pair asserts a property neither side writes with `var()` and nothing
 *     else competes for, which is decided by ordinary specificity. The code-block and `.selected`
 *     tests are `var()` on BOTH sides, so their `background` assertions are real specificity checks,
 *     not order coincidences.
 * Neither is a claim about a browser. Real rendering is checked by hand on a real display.
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
  // Both plain and selected rows are needed: padding and the selected fill can fail independently.
  const CHOICE_ROW = `<div class="mode-selector"><div class="session-choice"><button type="button" class="row row-choice"><span class="row-sign">›</span><span class="row-body">y</span></button><button type="button" class="row row-choice selected"><span class="row-sign">›</span><span class="row-body">z</span></button></div></div>`;
  // The band mode is a bare selector; `StatusBand.tsx` nests it under no footer wrapper.
  const MODE_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input">INPUT</span></div>`;
  const UNFOCUSED_INPUT_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input" data-focused="false">INPUT</span></div>`;

  // A grid's `1fr` goes to the second child, which can be the fatal banner rather than the
    // list. Then the list takes its content height and cannot scroll through a long dead session.
    // jsdom has no layout, so pin class-based growth: the list grows regardless of siblings,
    // and the activity line, composer and band remain outside its scrollable area.
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
    // Negative control: a grid hands the free space to the banner.
    const old = ".agent-ui-conversation { display: grid; grid-template-rows: auto 1fr auto auto; }";
    expect(computed(CONVERSATION(`<div class="fatal-error">e</div>`), ".agent-ui-conversation", old).display).toBe("grid");
  });

  /// The `?` overlay covers only the conversation. Its positioned scroller and absolute overlay
    /// jointly define the containing block; covering the whole panel hides its first heading
    /// under the winbar, making it unreadable and unreachable.
  it("keeps the ? overlay inside the list's own region, not over the bars below it", () => {
    const markup =
      `<div class="agent-ui-root agent-ui-conversation">` +
      `<div class="agent-ui-scroller"><div class="message-list">m</div>` +
      `<div class="keymap-overlay"><section><h2>This panel</h2></section></div></div>` +
      `<div class="activity-line">a</div><div class="status-band">s</div></div>`;
    expect(computed(markup, ".agent-ui-scroller").position).toBe("relative");
    expect(computed(markup, ".keymap-overlay").position).toBe("absolute");
    // The bars paint normally; nothing needs to out-stack the overlay.
    for (const bar of [".activity-line", ".status-band"]) {
      expect(computed(markup, bar).zIndex).toBe("auto");
    }
  });

  /** The unread pill sits on the band's right as inverted `↓N`, never floated over list text. */
  it("keeps the jump-to-bottom pill in normal flow inside the band, not floated over the list", () => {
    const markup = `<div class="status-band"><div class="band-right"><button class="band-seg band-unread">↓</button></div></div>`;
    expect(computed(markup, ".band-unread").position).toBe("static");
    expect(computed(markup, ".band-unread").background).toBe("var(--nv-fg)");
    expect(computed(markup, ".band-unread").color).toBe("var(--nv-bg)");
  });

  it("does not dim the which-key strip with opacity", () => {
    // Opacity lowers a guarded pair's contrast; `? keys` recedes by token instead.
    const rules = withoutComments.match(/\.which-key[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("paints a code block on --nv-surface even inside an assistant row", () => {
    expect(computed(CODE_BLOCK, "pre").background).toBe("var(--nv-surface)");
    // A competing rule proves this is a cascade win rather than an uncontested declaration.
    expect(computed(CODE_BLOCK, "pre", ".row-assistant pre { background: var(--nv-bg); }").background).toBe(
      "var(--nv-bg)",
    );
  });

  /* The conversation and foot fill the panel between viewport-based `--page-pad` margins.
     Neither measured widths nor container-query escapes are needed; size containers make
     WebKitGTK reset the reader's scroll position while a reply streams. */
  /* No width limit. The conversation and the foot fill the panel less `--page-pad`
     at each side, at every width, and nothing at the list's foot is drawn over the rows. */
  it("fills the panel at every width: no column cap, one margin for the list and the foot, no wide block, no fade", () => {
    expect(withoutComments).not.toMatch(/--row-inline-size|--list-inline-size|--column-w|--wide-cap|--column-max/);
    const rules = splitRules(withoutComments);
    // Nothing at a wide breakpoint gives code or a table a width of its own.
    const wide = rules.filter((r) => r.parent === "@media (min-width: 1100px)" && /code-block|table-scroll/.test(r.selector));
    expect(wide).toEqual([]);
    expect(withoutComments).toMatch(/--page-pad:\s*clamp\(18px, 3\.5vw, 48px\);/);
    const SIDE = "var(--page-pad)";
    const declarationsOf = (selector: string) =>
      rules.filter((r) => r.parent === null && r.selector.split(",").map((b) => b.trim()).includes(selector)).map((r) => r.declarations).join(";");
    // The air under the last row is the list's own bottom padding; nothing is laid over the rows there.
    expect(declarationsOf(".message-list")).toContain(`padding: 28px ${SIDE} 24px`);
    expect(rules.some((r) => /\.message-list::after/.test(r.selector))).toBe(false);
    expect(withoutComments).not.toMatch(/--list-foot|--list-fade|data-more-below/);
    for (const foot of [".activity-line", ".queue-lines", ".turn-changes"]) {
      expect(declarationsOf(foot), foot).toContain(SIDE);
    }
    expect(declarationsOf(".agent-ui-conversation > .row-error")).toContain(`margin: 8px ${SIDE}`);
    // The composer is centred with the same margin on each side.
    expect(declarationsOf(".composer")).toContain(`margin: 10px ${SIDE} 14px;`);
    // None of them is centred by a width and an auto margin instead: that would centre the list's box, not
    // its rows, and leave the foot and the conversation on different edges.
    const FILLS = [".message-list > .row", ".message-list > :where(.subagent-child) > .row", ".turn-time", ".activity-line", ".queue-lines", ".turn-changes", ".composer", ".agent-ui-conversation > .row", ".history-notice"];
    for (const rule of rules.filter((r) => r.parent === null)) {
      for (const branch of rule.selector.split(",").map((b) => b.trim())) {
        if (!FILLS.includes(branch)) continue;
        expect(rule.declarations, branch).not.toMatch(/(?:^|;)\s*(?:max-)?width\s*:/);
        expect(rule.declarations, branch).not.toMatch(/margin(?:-inline)?\s*:[^;]*\bauto\b/);
      }
    }
    // Nothing gives these a width or a negative margin of their own: they fill the row.
    const LEAVES = [".code-block", ".tool-result-body", ".tool-card-generic", ".permission-card-edit", ".permission-card-input"];
    for (const rule of rules.filter((r) => r.parent === null)) {
      for (const branch of rule.selector.split(",").map((b) => b.trim())) {
        if (!LEAVES.includes(branch)) continue;
        expect(rule.declarations, branch).not.toMatch(/(?:^|;)\s*width\s*:/);
        expect(rule.declarations, branch).not.toMatch(/margin-left\s*:\s*-/);
      }
    }
    // A reply's fenced block is as wide as its lines, between 36rem (or the row) and the row; the bare
    // `.code-block` above still takes no width, so a block elsewhere fills its row.
    const reply = rules.find((r) => r.parent === null && r.selector === ".row-assistant .code-block")!;
    expect(reply.declarations).not.toMatch(/(?:^|;)\s*width:/);
    expect(reply.declarations).not.toMatch(/min-width:/);
    expect(reply.declarations).toMatch(/max-width:\s*100%;/);
    expect(rules.find((r) => r.parent === null && r.selector === ".code-block")!.declarations).toMatch(/overflow-x:\s*auto;/);
    // Positive control: the scan really walked those rules.
    expect(rules.some((r) => r.parent === null && r.selector.split(",").some((b) => b.trim() === ".code-block"))).toBe(true);
    expect(rules.some((r) => r.parent === null && r.selector.split(",").some((b) => b.trim() === ".composer"))).toBe(true);
    // Negative control: the scan above sees a width put back on one of them.
    const back = splitRules(".code-block { width: max(100%, 40rem); }")[0];
    expect(back.declarations).toMatch(/(?:^|;)\s*width\s*:/);
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

  /* The in-flight indicator must stay visible on a narrow line. jsdom runs no flex layout,
     so inspect the declarations that allocate the available width. */

  it("keeps the star from being squeezed off a narrow activity line", () => {
    // Flex items shrink by default. The star is the line's one moving mark and must stay
    // visible even in a narrow panel, so it uses `flex: none`, as `.diff-gutter` does.
    const html =
      `<div class="activity-line"><span class="activity-glyph" data-moving="true">✻</span>` +
      `<span class="turn-activity" data-phase="thinking"><span class="turn-state">thinking</span></span></div>`;
    const glyph = computed(html, ".activity-glyph");
    expect(glyph.flexShrink).toBe("0");
    expect(glyph.flexGrow).toBe("0");
    const gutter = computed(`<div class="diff-line"><span class="diff-gutter">+</span></div>`, ".diff-gutter");
    expect(gutter.flexShrink).toBe("0");
    // Negative control: the default `flex-shrink: 1` put back is reachable from any later rule.
    expect(computed(html, ".activity-glyph", ".activity-glyph { flex: 1 1 auto; }").flexShrink).toBe("1");
  });

  /** Font-size arithmetic reads only top-level `ROOT_TOKENS`. A literal override nested under
   *  `@media` on `:root:not([data-theme="light"])` could pin sizes despite `agent.font_size`.
   *  Keep that arithmetic trustworthy by requiring these tokens to be declared nowhere else,
   *  using the brace-aware `allCustomPropertyDeclarations` walker to see every scope. */
  it("declares every panel token exactly once, on a plain :root, with nowhere else for one to hide", () => {
    const PANEL_TOKEN = /^--(row|prose|prompt|fs)-/;
    const declared = allCustomPropertyDeclarations(withoutComments).filter((d) => PANEL_TOKEN.test(d.name));
    // A premise, not a formality: if the walker stopped finding them this test would pass empty.
    expect(declared.length, "the panel's own tokens are not being found at all").toBeGreaterThanOrEqual(11);

    const sites = new Map<string, string[]>();
    for (const d of declared) sites.set(d.name, [...(sites.get(d.name) ?? []), d.selector.trim()]);
    // Every panel token is a `:root` knob; no measured row-width exception is allowed.
    for (const [name, selectors] of sites) {
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
    // ...and `.row` declares no custom property a guard here would have to know about.
    expect([...ROW_TOKENS.keys()]).toEqual([]);
  });

  /* One Rust-emitted font-size base drives the scale. Require tokens at every font-size use,
     evaluate their ratios as pixel values, hold the literal fallback equal to Rust's default,
     and check that Rust actually emits the fallback's token name. */

  it("spends every font-size as a var(--fs-*) ratio, never a literal length", () => {
    // A whole scale is worth nothing if one call site still hardcodes a
    // length; that one rule would not move when the user changes `agent.font_size`, and nothing
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
    // Evaluate the ratios rather than matching their spelling: transposed digits can retain
    // the same text fragments while producing the wrong size.
    // The base is the editor's own text size by default and only the status band spends it, so the band
    // lines up with lualine; the chat reads at twelve fourteenths of it.
    const expected: [string, number][] = [
      ["--fs-xs", 9.5],
      ["--fs-sm", 10.5],
      ["--fs-code", 12],
      ["--fs-md", 11],
      ["--fs-base", 14],
      ["--fs-prose", 12],
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

    // The heading is one twentieth over the reply's prose, by the same arithmetic.
    expect(evaluateFsExpression(ROOT_TOKENS.get("--fs-heading")!)).toBeCloseTo(12 * 1.05, 10);
  });

  it("keeps the 14px fallback equal to Rust's own DEFAULT_PANEL_FONT_SIZE_PX, not a second hand-copied number", () => {
    // Derive the fallback from Rust's source: copying `14` into the assertion would add a third
    // independent copy of the number this equality is meant to keep in sync.
    const rustConstant = tokensRs.match(/pub const DEFAULT_PANEL_FONT_SIZE_PX:\s*f32\s*=\s*([\d.]+);/);
    expect(rustConstant, "DEFAULT_PANEL_FONT_SIZE_PX not found in tokens.rs -- the parse is broken, not the CSS").not.toBeNull();
    const cssFallback = ROOT_TOKENS.get("--fs-base")!.match(/^var\(--nv-font-size,\s*([\d.]+)px\)$/);
    expect(cssFallback, "--fs-base's fallback is not the plain var(--nv-font-size, <n>px) shape this test expects").not.toBeNull();
    expect(parseFloat(cssFallback![1])).toBe(parseFloat(rustConstant![1]));

  });

  it("reads --nv-font-size's fallback spelling, not just its bare form, as a name Rust emits", () => {
    // The shared name regex stops at the fallback's comma, so `var(--nv-font-size, 14px)`
    // still yields only the name. Pin the real regex rather than assume fallback syntax works.
    const used = Array.from(withoutComments.matchAll(/var\((--nv-[a-z-]+)/g), (m) => m[1]);
    expect(used).toContain("--nv-font-size");
    // And the isolated case, so a future change to the SHARED regex that breaks this is caught here
    // even if `--nv-font-size` itself were ever removed from the real file.
    expect(Array.from("var(--nv-font-size, 14px)".matchAll(/var\((--nv-[a-z-]+)/g), (m) => m[1])).toEqual([
      "--nv-font-size",
    ]);
  });

  it("spends the phase word before the Stop button as the activity line narrows", () => {
    // Only `.turn-activity` yields space; inside it only the phase word yields. The star, clock
    // and Stop stay intact. Stop is the mouse user's only interrupt control, so losing it past
    // the right edge removes that affordance.
    const html =
      `<div class="activity-line">` +
      `<span class="activity-glyph" data-moving="true">✻</span>` +
      `<span class="turn-activity" data-phase="tool">` +
      `<span class="turn-state">running NotebookEdit</span><span class="turn-elapsed">123s+</span></span>` +
      `<button type="button" class="stop">Stop</button></div>`;
    const shrink = (selector: string) => Number(computed(html, selector).flexShrink);

    const dom = new DOMParser().parseFromString(html, "text/html");
    expect(dom.querySelector(".turn-state")?.parentElement?.className).toBe("turn-activity");

    // `.turn-activity` is the only child of `.activity-line` that can give; Stop never does.
    expect(shrink(".turn-activity")).toBeGreaterThan(0);
    expect(shrink(".stop")).toBe(0);

    // The star before it never gives either. Inside `.turn-activity`: the phase word is the ONLY item
    // there that can give, so `.turn-activity` shrinking IS the phase word truncating.
    expect(shrink(".activity-glyph")).toBe(0);
    expect(shrink(".turn-elapsed")).toBe(0);
    expect(shrink(".turn-state")).toBeGreaterThan(0);

    // ...and what `flex: none` on the clock actually buys: the DISTRIBUTION never spends it. It does
    // not mean it can never be clipped -- once the word is gone there is nothing left inside the box
    // to give -- so `.turn-activity` clips its own overflow rather than letting the clock run out
    // over Stop.
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
    // Negative control: nothing shrinking or clipping pushes Stop off the edge.
    expect(computed(html, ".turn-state", ".activity-line .turn-activity .turn-state { flex: none; }").flexShrink).toBe("0");
  });


  it("keeps the sign column on the same track whether a grid row's body is prose or a wide code block", () => {
    // Outside the conversation, the sign grid must keep column 1 at the same x position even
    // when a body's code block is wide. jsdom returns unresolved `var()` text, so `expandVars`
    // resolves the real root declarations; comparing unresolved text could pass if the row
    // selector stopped applying.
    const proseRow =
      `<div class="review-overlay"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><p>prose</p></div></div></div>`;
    const codeRow =
      `<div class="review-overlay"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const proseColumns = expandVars(computed(proseRow, ".row").gridTemplateColumns);
    expect(proseColumns).toBe("20px minmax(0, 1fr)");
    expect(expandVars(computed(codeRow, ".row").gridTemplateColumns)).toBe(proseColumns);
    // A later rule at the row's specificity can change column 1. Its literal override passes
    // unchanged through `expandVars` and must differ from `proseColumns`.
    expect(
      expandVars(
        computed(codeRow, ".row", ".row-assistant { grid-template-columns: 24px minmax(0, 62ch); }").gridTemplateColumns,
      ),
    ).not.toBe(proseColumns);
  });

  it("keeps the grid for rows outside the conversation and gives the conversation's rows a sign cell and a body of their own, with no size container", () => {
    // A row outside the conversation (the review overlay's) is a grid of a sign cell and a body. The
    // conversation's rows are blocks, and a tool or thinking row draws its sign as generated content.
    // No row is a size container: one makes WebKitGTK drop the reader while a reply streams. And no
    // row spends a measured width.
    const grid = computed(
      `<div class="review-overlay"><div class="row row-review-file"><span class="row-sign">▾</span><div class="row-body">f</div></div></div>`,
      ".row",
    );
    expect(grid.getPropertyValue("container-type")).toBe("normal");
    expect(grid.display).toBe("grid");
    expect(expandVars(grid.gridTemplateColumns)).toBe("20px minmax(0, 1fr)");
    expect(grid.borderLeftWidth).toBe("0px");
    expect(ROW_TOKENS.size).toBe(0);
    const html = `<div class="message-list"><div class="row row-assistant" data-sign=""><span class="row-sign"></span><div class="row-body">x</div></div></div>`;
    const row = computed(html, ".row");
    expect(row.getPropertyValue("container-type")).toBe("normal");
    expect(row.display).toBe("block");
    expect(row.gridTemplateColumns).toBe("var(--row-sign-w) minmax(0, var(--prose-measure))");
    expect(row.borderLeftWidth).toBe("0px");
    expect(row.maxWidth).toBe("none");
    expect(computed(html, ".row-sign").display).toBe("none");
    expect(computed(html, ".row-body").gridColumn).toMatch(/^(auto|)$/);
    // The sign is `data-sign` drawn as generated content, in the first column.
    const sign = splitRules(withoutComments).find((r) => r.selector.includes(".message-list > .row-tool::before"));
    expect(sign?.declarations).toMatch(/content:\s*attr\(data-sign\) \/ "";/);
    expect(sign?.declarations).toMatch(/position:\s*absolute;/);
    // The sign cell is two characters of the mono face at the sign's size, written from `--fs-code` so it is
    // one width wherever it is spent, and the body edge is that and the gap.
    expect(ROOT_TOKENS.get("--sign-w")).toBe("calc(var(--fs-code) * 1.2)");
    expect(ROOT_TOKENS.has("--body-edge")).toBe(false);
    // Negative control: the container put back is exactly what this and the tripwire refuse.
    expect(computed(html, ".row", ".row { container-type: inline-size; }").getPropertyValue("container-type")).toBe("inline-size");
  });

  it("leaves a choice row on the sign-column grid, not on the boxed-button padding", () => {
    const row = computed(CHOICE_ROW, "button");
    expect(row.display).toBe("grid");
    expect(expandVars(row.gridTemplateColumns)).toBe("20px minmax(0, 1fr)");
    // `.mode-selector button:not(.row-choice)`'s 10px would show up here; `.row`'s own is `2px 0`
    // and `.row-choice` narrows it to `4px 0`, so the left padding is the tell.
    expect(row.paddingLeft).toBe("0px");
    const clobbered = computed(CHOICE_ROW, "button", ".mode-selector button { display: block; padding: 10px; }");
    expect(clobbered.display).toBe("block");
    expect(clobbered.paddingLeft).toBe("10px");
  });

  /* Derive the sign gutter from the sign width and gap, and require the review overlay's
     indented lines to spend it. Independent literal sums can drift out of alignment. Container
     query units are also forbidden because of the WebKitGTK scroll-reset failure. */

  it("derives --row-gutter from the sign column instead of copying its sum, and every indent that lines up with the grid spends it", () => {
    // Half 1: the declared value is a formula over the two knobs that make up the sign column, not a
    // copy of their sum -- `calc(28px)` would satisfy every other test in this file just as well.
    const gutter = ROOT_TOKENS.get("--row-gutter");
    expect(gutter).toBe("calc(var(--row-sign-w) + var(--row-gap))");
    expect(gutter).toMatch(/var\(--row-sign-w\)/);
    expect(gutter).toMatch(/var\(--row-gap\)/);
    // A formula sitting unused proves nothing: review lines must spend it, and no calc may
    // copy the sign width plus gap as a literal sum.
    expect(winningDeclaration('<div class="review-comment-text">note</div>', ".review-comment-text", "padding-left")).toBe("var(--row-gutter)");
    const consumers = splitRules(withoutComments).filter((r) => /var\(--row-gutter\)/.test(r.declarations));
    expect(consumers.length).toBeGreaterThanOrEqual(1);
    for (const rule of consumers) {
      for (const branch of rule.selector.split(",").map((b) => b.trim())) expect(branch).toMatch(/^\.review-/);
    }
    expect(withoutComments).not.toMatch(/\b(?:28|30|32)px\)/);
    // Negative control: a hand-copied sum in a calc is what the scan above is for.
    expect(".review-comment-text { padding-left: calc(28px); }").toMatch(/\b(?:28|30|32)px\)/);
  });
  it("keeps a container-query unit out of every custom property declaration (WebKitGTK guard)", () => {
    // There is no container-query unit anywhere in the file (the tripwire near the top of the
    // `index.css` block: with no container, a `cq*` unit would silently resolve against the
    // viewport). This guard pins what that means for custom properties in particular: a `var()`
    // substitutes as raw tokens, so a `cqw` unit written INSIDE a custom property's value still
    // substitutes as raw tokens, container units inside custom properties are exactly the corner
    // where an engine has shipped bugs, this panel runs in WebKitGTK, and the failure mode is not a
    // wrong number in one place, it is the whole panel resolving against the wrong axis. So no
    // `--custom-property`'s declared value may contain a `cq*` unit at all -- wherever in the file it
    // is declared, not merely inside an unqualified top-level `:root`.
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
    // A nested override on `:root:not(...)` must be seen outside the top-level root.
    const bypass =
      '@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --row-sign-w: 10cqw; } }';
    expect(allCustomPropertyDeclarations(bypass)).toEqual([
      { name: "--row-sign-w", value: "10cqw", selector: ":root:not([data-theme=\"light\"])" },
    ]);
    // The root-only regex cannot see this override.
    expect(rootCustomProperties(bypass).size).toBe(0);

    // Multiline values must also be visible: a bare `100cqw` inside a three-line calc is still
    // a container-query escape. DotAll keeps it in the scan.
    const multiline = ":root {\n  --bypass: calc(\n    100cqw\n  );\n}";
    const multilineDecls = allCustomPropertyDeclarations(multiline);
    expect(multilineDecls).toEqual([{ name: "--bypass", value: "calc(\n    100cqw\n  )", selector: ":root" }]);
    // Finding the declaration must expose its forbidden value, not exempt it.
    expect(multilineDecls[0].value).toMatch(/cq(w|h|i|b|min|max)/i);
    // Removing only dotAll demonstrates why a multiline value can disappear.
    const preFixRegex = /^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/;
    expect("\n  --bypass: calc(\n    100cqw\n  )\n".match(preFixRegex)).toBeNull();
  });


  /* --- your own message ----------------------------------------------------------------------------------
     Your message is the shape every mainstream agent UI uses: a tinted block on the right. These tests pin
     that shape, the cascade that keeps it, and two properties any shape must keep -- the text colour is
     guarded against the fill under it (the tint's own `--nv-tint-fg`), and a long unbroken token breaks
     inside the block instead of scrolling the conversation. */
  it("draws your message as a tinted bubble at the body's edge, its sign outside it, and the cascade lets it win", () => {
    const html = '<div class="message-list"><div class="row row-prompt"><div class="row-sign">›</div><div class="row-body">hello</div></div></div>';
    expect(computed(html, ".row-prompt").display).toBe("flex");
    expect(computed(html, ".row-prompt").justifyContent).toBe("flex-end");
    expect(computed(html, ".row-body").background).toBe("var(--nv-tint)");
    expect(computed(html, ".row-body").color).toBe("var(--nv-tint-fg)");
    expect(computed(html, ".row-body").borderRadius).toBe("18px 18px 5px 18px");
    expect(computed(html, ".row-sign").display).toBe("none");
    expect(winningDeclaration(html, ".row-body", "background", ".row-prompt .row-body { background: none; }")).toBe("none");
    // One unbroken token must break inside the bubble instead of scrolling the conversation sideways: the
    // body states both wrapping properties itself, so the list's own `overflow-x: hidden` cannot turn an
    // overflow into a silent clip.
    const live = computed(html, ".row-prompt .row-body");
    expect(live.whiteSpace).toBe("pre-wrap");
    expect(live.overflowWrap).toBe("anywhere");
    const bodyRule = splitRules(withoutComments).find((r) => r.selector === ".row-prompt .row-body")!;
    expect(bodyRule.declarations).toMatch(/white-space:\s*pre-wrap;/);
    expect(bodyRule.declarations).toMatch(/overflow-wrap:\s*anywhere;/);
    // Negative controls: a later rule, and a lower-specificity `!important` one, each take the wrapping away.
    expect(computed(html, ".row-prompt .row-body", ".row-prompt .row-body { overflow-wrap: normal; }").overflowWrap).toBe("normal");
    expect(computed(html, ".row-prompt .row-body", ".row-body { white-space: nowrap !important; }").whiteSpace).toBe("nowrap");
  });
  /* The things a reader reviews or answers are framed and nothing else is -- an edit, a
     failure's output (pinned with the connector test), the permission card, a code block, and your own
     messages and the box you write them in. Prose, thoughts and a successful tool line stay unframed. */
  it("frames an edit in the transcript with its counts at the right edge and its diff under a hairline, and no connector", () => {
    const html =
      `<div class="message-list"><div class="row row-tool"><div class="row-body"><div class="tool-call">` +
      `<div class="permission-card-edit"><div class="permission-card-edit-head"><span class="permission-card-edit-counts">+1</span></div>` +
      `<pre class="permission-card-diff"></pre></div></div></div></div></div>`;
    expect(winningDeclaration(html, ".permission-card-edit", "border")).toBe("1px solid var(--rule-frame)");
    expect(winningDeclaration(html, ".permission-card-edit", "border-radius")).toBe("10px");
    // As wide as its head or diff, as a reply's code block is: at least 36rem (or the row), at most the row.
    expect(winningDeclaration(html, ".permission-card-edit", "width")).toBeNull();
    expect(winningDeclaration(html, ".permission-card-edit", "min-width")).toBe("0px");
    expect(winningDeclaration(html, ".permission-card-edit", "max-width")).toBe("100%");
    expect(winningDeclaration(html, ".permission-card-edit-counts", "margin-left")).toBe("auto");
    expect(winningDeclaration(html, ".permission-card-edit-head", "border-bottom")).toBe("1px solid var(--rule-soft)");
    expect(winningDeclaration(html, ".permission-card-diff", "margin-left")).toBe("0px");
    expect(splitRules(withoutComments).some((r) => /permission-card-diff::before/.test(r.selector))).toBe(false);
    // Inside a permission card the same view has no frame: the card's own is the one around it.
    const inCard = `<div class="permission-card"><div class="permission-card-edit"></div></div>`;
    expect(winningDeclaration(inCard, ".permission-card-edit", "border-color")).toBe("var(--nv-warn)");
    expect(ROOT_TOKENS.get("--rule-frame")).toBe("color-mix(in srgb, var(--nv-fg) 24%, var(--nv-bg))");
  });

  it("outlines the permission card in the warn colour and a code block in the hairline, and leaves prose and thoughts unframed", () => {
    expect(winningDeclaration(`<div class="permission-card"></div>`, ".permission-card", "border")).toBe("1px solid var(--nv-warn)");
    expect(winningDeclaration(`<pre class="code-block"></pre>`, ".code-block", "border")).toBe("1px solid var(--rule-soft)");
    for (const kind of ["row-assistant", "row-thinking", "row-tool"]) {
      const html = `<div class="message-list"><div class="row ${kind}"><div class="row-body">x</div></div></div>`;
      expect(winningDeclaration(html, ".row-body", "border"), kind).toBeFalsy();
      expect(winningDeclaration(html, ".row-body", "background"), kind).toBeFalsy();
    }
  });

  it("puts a queued line's mark in the sign column and its words at the body's edge", () => {
    const html = '<div class="queue-lines"><div class="queue-line"><span class="queue-glyph">⧗</span><span class="queue-text">x</span></div></div>';
    expect(computed(html, ".queue-line").display).toBe("flex");
    expect(computed(html, ".queue-line").gap).toBe("8px");
    expect(winningDeclaration(html, ".queue-text", "text-overflow")).toBe("ellipsis");
  });
  it("opens a turn with air above your message, and keeps back-to-back messages together", () => {
    const list = (rows: string) => `<div class="message-list">${rows}</div>`;
    const prompt = (t: string) => `<div class="row row-prompt"><div class="row-body">${t}</div></div>`;
    const reply = `<div class="row row-assistant"><div class="row-body">r</div></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = list(prompt("a") + reply + prompt("b") + prompt("c"));
    const [, second, third] = Array.from(document.querySelectorAll<HTMLElement>(".row-prompt, .row-assistant"));
    const promptAfterReply = document.querySelectorAll<HTMLElement>(".row-prompt")[1];
    const backToBack = document.querySelectorAll<HTMLElement>(".row-prompt")[2];
    expect(second.classList.contains("row-assistant")).toBe(true);
    expect(third).toBe(promptAfterReply);
    // `winningDeclarationOn` reads `@media` rules as if they applied (it flattens them on purpose), so
    // this is the wide panel's opening margin -- and it still must not reach the back-to-back case.
    expect(winningDeclarationOn(promptAfterReply, "margin-top")).toBe("32px");
    expect(winningDeclarationOn(backToBack, "margin-top")).toBe("8px");
    // The reply after a message gets a little more room than two agent rows do.
    expect(winningDeclarationOn(second, "margin-top")).toBe("24px");
    // The back-to-back rule outranks both opening rules by specificity, not by where it sits.
    expect(compareSpecificity(specificity(".message-list > .row-prompt + .row-prompt:not(:first-child)"), specificity(".message-list > .row-prompt:not(:first-child)"))).toBeGreaterThan(0);
  });

  it("gives a message read inside the running turn the ordinary gap, not a turn's opening air", () => {
    const delivered = `<div class="row row-prompt row-prompt-delivered"><div class="row-body">and the tests</div></div>`;
    const reply = `<div class="row row-assistant"><div class="row-body">r</div></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = `<div class="message-list">${reply}${delivered}</div>`;
    const row = document.querySelector<HTMLElement>(".row-prompt-delivered")!;
    // Flattened `@media` included: the wide panel's 34px opening must not reach it either.
    expect(winningDeclarationOn(row, "margin-top")).toBe("12px");
    expect(compareSpecificity(specificity(".message-list > .row-prompt.row-prompt-delivered:not(:first-child)"), specificity(".message-list > .row-prompt:not(:first-child)"))).toBeGreaterThan(0);
  });

  it("draws no sign cell in the conversation, and keeps the sign-cell cursor on the grid outside it", () => {
    const inList = `<div class="message-list" data-focused="true"><div class="row row-prompt row-current"><div class="row-sign">›</div><div class="row-body">hi</div></div></div>`;
    expect(computed(inList, ".row-sign").display).toBe("none");
    const review = `<div class="review-overlay"><div class="row row-review-file row-current"><div class="row-sign">▾</div><div class="row-body">f</div></div></div>`;
    const sign = computed(review, ".row-sign");
    expect(sign.display).not.toBe("none");
    expect(sign.background).toBe("var(--nv-fg)");
    expect(sign.color).toBe("var(--nv-bg)");
    // Negative control: a rule hiding every sign cell would take the overlay's cursor with it.
    expect(computed(review, ".row-sign", ".row-sign { display: none; }").display).toBe("none");
  });

  it("draws a delegation's nested rows as the conversation draws its own, under the group's indent", () => {
    // A nested row sits one wrapper deeper than the list's own rows (the wrapper carries the indent), so every
    // rule that takes a row off the sign-cell grid has to reach it too, or its body falls into the 20px sign column.
    const nested = (cls: string, sign = "✓") => `<div class="message-list"><div class="row row-tool" data-sign="✓"><span class="row-sign"></span><div class="row-body">general-purpose(B)</div></div>` +
      `<div class="subagent-child" style="margin-inline-start: 1.5em" data-depth="1"><div class="row ${cls}" data-sign="${sign}"><span class="row-sign"></span><div class="row-body">Read: /a/long/path</div></div></div></div>`;
    for (const cls of ["row-tool", "row-tool-run", "row-thinking", "row-assistant"]) {
      const row = computed(nested(cls), ".subagent-child > .row");
      expect(row.display, cls).toBe("block");
      expect(computed(nested(cls), ".subagent-child .row-sign").display, cls).toBe("none");
    }
    for (const cls of ["row-tool", "row-tool-run", "row-thinking"]) {
      expect(computed(nested(cls), ".subagent-child > .row").paddingLeft, cls).toBe("24px");
      expect(computed(nested(cls), ".subagent-child > .row").position, `${cls} holds its positioned sign`).toBe("relative");
    }
    expect(computed(nested("row-assistant"), ".subagent-child > .row").position, "a row with no sign is not positioned").toBe("static");
    expect(computed(nested("row-tool"), ".subagent-child > .row").marginTop).toBe("12px");
    expect(computed(nested("row-assistant"), ".subagent-child > .row").marginTop).toBe("24px");
    // The mark is drawn in the nested row's own indent, as in the list's rows, with the same weight as theirs so
    // the running and failed colours still win.
    const sign = splitRules(stripComments(css)).find((r) => r.selector.split(",").map((b) => b.trim()).includes(".message-list > :where(.subagent-child) > .row-tool::before"));
    expect(sign?.declarations).toMatch(/content:\s*attr\(data-sign\) \/ "";/);
    expect(sign?.declarations).toMatch(/position:\s*absolute;/);
  });

  it("keeps the band's background-task count apart from the segment before it", () => {
    // The count is a button of its own after the details button, whose gap does not reach it.
    const band = `<div class="status-band"><button class="band-open"><span class="band-seg">manual</span></button><button type="button" class="band-seg band-background">1 background task</button></div>`;
    expect(computed(band, "button.band-background").marginLeft).toBe("8px");
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
    // One class plus one attribute on the bare mode block: specificity (0,2,0).
    expect(specificity('.band-mode[data-mode="input"]')).toEqual([0, 2, 0]);
    expect(compareSpecificity([0, 3, 0], [0, 2, 0])).toBeGreaterThan(0);
  });

  // The empty screen has no boxed mode-selector buttons, hence no such rule competes with
    // `.row-choice`. Its padding, plain background, hover and selected fills are checked separately.

  it("paints the selected choice row's own fill, not the old per-list selected rule's", () => {
    // `.mode-selector .session-choice button.selected` (0,3,1) outweighs
    // `.row-choice.selected` (0,2,0); the selected fixture must expose that competing fill.
    const row = computed(CHOICE_ROW, "button.selected");
    expect(row.background).toBe("var(--nv-cursorline)");
    const clobbered = computed(
      CHOICE_ROW,
      "button.selected",
      ".mode-selector .session-choice button.selected { background: var(--nv-bg); }",
    );
    expect(clobbered.background).toBe("var(--nv-bg)");
  });

  /* The band's mode block and the editor's statusline mode
     section beside it look the same; only the word differs. */
  /* With details to open, `.band-open` is a real `<button>`: it would keep the global 6px radius, and its
     `overflow: hidden` would then cut the two left corners of the mode block, which fills it from the band's edge. */
  it("keeps the mode block's left corners square inside the band's details button", () => {
    const band = `<div class="status-band"><button type="button" class="band-open"><span class="band-seg band-mode" data-mode="input" data-focused="true">INPUT</span></button></div>`;
    expect(winningDeclaration(band, ".band-open", "overflow")).toBe("hidden");
    expect(winningDeclaration(band, ".band-open", "border-radius")).toMatch(/^0(px)?$/);
    // The same button outside the band keeps the global radius, so the check reads the real cascade.
    expect(winningDeclaration(`<button type="button" class="band-open"></button>`, ".band-open", "border-radius")).toBe("6px");
  });

  /* These blocks take the keys whole by `.focus()` from a keystroke, which WebKit answers with its own focus ring:
     a thick accent line outside the block's frame, in a colour no theme chose. Their rows or their frame show where
     the keys are, so the ring is turned off on the block itself and a field inside keeps its own. */
  it("draws no default focus ring on a block that takes the keys whole", () => {
    for (const block of ["question-dock", "plan-dock", "chooser", "slash-picker", "tasks-overlay"]) {
      document.head.innerHTML = `<style>${css}</style>`;
      document.body.innerHTML = `<div class="${block}" tabindex="0"></div><div class="${block}" tabindex="0"></div>`;
      const [el, unfocused] = Array.from(document.body.children);
      // The selector engine remembers its answer per selector, so nothing may ask about `el` before it has focus.
      (el as HTMLElement).focus();
      expect(document.activeElement).toBe(el);
      expect(winningDeclarationOn(el, "outline"), block).toBe("none");
      expect(winningDeclarationOn(unfocused, "outline"), block).toBeNull();
    }
  });

  /* The dock borrows the composer's slot and its colour, but the composer's ring is two lines deep. The dock's list
     has its own cursor band showing where the keys are, so one line is all its frame has to say. */
  it("frames the active question dock in a single line of the focus ring's colour", () => {
    const dock = `<div class="question-dock" data-active="true" tabindex="0"></div>`;
    expect(winningDeclaration(dock, ".question-dock", "border-color")).toBe("var(--nv-focus-ring)");
    expect(winningDeclaration(dock, ".question-dock", "box-shadow")).toBeNull();
    expect(winningDeclaration(`<div class="question-dock" data-active="false"></div>`, ".question-dock", "border-color")).toBeNull();
  });

  it("paints the mode block in the statusline's own mode colours, closed by an arrow, and nothing without the keys", () => {
    const block = (mode: string, focused = "true") =>
      `<div class="status-band"><div class="band-open"><span class="band-seg band-mode" data-mode="${mode}" data-focused="${focused}">X</span></div></div>`;
    for (const [mode, status] of [["browse", "normal"], ["hint", "normal"], ["input", "insert"], ["caret", "visual"], ["visual", "visual"], ["vline", "visual"]]) {
      expect(winningDeclaration(block(mode), ".band-mode", "background"), mode).toBe(`var(--nv-status-${status}-bg)`);
      expect(winningDeclaration(block(mode), ".band-mode", "color"), mode).toBe(`var(--nv-status-${status}-fg)`);
    }
    // Bold, one cell of air each side, the row's full height, from the band's own left edge.
    expect(winningDeclaration(block("browse"), ".band-mode", "font-weight")).toBe("700");
    expect(winningDeclaration(block("browse"), ".band-mode", "padding")).toMatch(/^0(px)? 1ch$/);
    expect(winningDeclaration(block("browse"), ".band-mode", "align-self")).toBe("stretch");
    expect(winningDeclaration(block("browse"), ".status-band", "padding")).toMatch(/^0(px)? var\(--page-pad\) 0(px)? 0(px)?$/);
    // The separator: a triangle one cell wide in the block's own fill, in the cell the block leaves for it.
    const arrow = splitRules(withoutComments).find((r) => r.parent === null && r.selector === ".band-mode::after")!;
    expect(arrow.declarations).toMatch(/background:\s*inherit;/);
    expect(arrow.declarations).toMatch(/clip-path:\s*polygon\(0 0, 100% 50%, 0 100%\);/);
    expect(arrow.declarations).toMatch(/width:\s*1ch;/);
    expect(winningDeclaration(block("browse"), ".band-mode", "margin-right")).toBe("1ch");
    // Without the keys: no fill or arrow in any mode. An inactive statusline has no mode
    // section; its place stays so nothing shifts or looks like an empty rendering artifact.
    for (const mode of ["browse", "input", "visual"]) {
      expect(winningDeclaration(block(mode, "false"), ".band-mode", "background"), mode).toBe("none");
      expect(winningDeclaration(block(mode, "false"), ".band-mode", "color"), mode).toBe("var(--nv-chrome-muted)");
      expect(winningDeclaration(block(mode, "false"), ".band-mode", "padding"), mode).toMatch(/^0(px)? 1ch$/);
    }
    const hiddenArrow = splitRules(withoutComments).find((r) => r.parent === null && r.selector === '.band-mode[data-focused="false"]::after')!;
    expect(hiddenArrow.declarations).toMatch(/display:\s*none;/);
    // The guard admits a status fg only on its own mode's fill: a mismatched pair is refused.
    expect(unguardedTextColorDeclarations(".band-mode { background: var(--nv-status-normal-bg); color: var(--nv-status-insert-fg); }")).toEqual([
      "color: var(--nv-status-insert-fg);",
    ]);
    expect(unguardedTextColorDeclarations(".band-open { background: var(--nv-status-normal-bg); color: var(--nv-status-normal-fg); }")).toEqual([
      "color: var(--nv-status-normal-fg);",
    ]);
    expect(unguardedTextColorDeclarations('.band-mode[data-mode="input"] { background: var(--nv-status-insert-bg); color: var(--nv-status-insert-fg); }')).toEqual([]);
  });

  /* The start screen's box sits where the conversation's box will be, so a first send moves nothing. */
  it("puts the start screen's box at the foot, under the rows, at the conversation box's distance from the band", () => {
    const rule = (selector: string) => splitRules(withoutComments).find((r) => r.parent === null && r.selector === selector)!;
    expect(rule(".dashboard > .queue-lines, .dashboard > .dash-input").declarations).toMatch(/order:\s*3;/);
    expect(rule(".dashboard > .dash-items, .dashboard > .dash-context").declarations).toMatch(/order:\s*1;/);
    expect(rule(".dashboard::after").declarations).toMatch(/order:\s*2;/);
    expect(rule(".empty-box-stack").declarations).toMatch(/margin:\s*auto 0 0;/);
    // The foot under the box is the conversation composer's bottom margin.
    expect(rule(".empty-tab").declarations).toMatch(/padding:\s*18px var\(--page-pad\) 14px;/);
    expect(rule(".composer").declarations).toMatch(/margin:\s*10px var\(--page-pad\) 14px;/);
  });

  it("keeps the dashboard's entry-key pointer muted, passive and on one reserved line with an ellipsis", () => {
    const html = '<div class="dash-items"><div class="dash-entry-slot"><div class="dash-entry-hint">Ctrl+l to use these keys</div></div></div>';
    expect(winningDeclaration(html, ".dash-entry-hint", "color")).toBe("var(--nv-muted)");
    expect(winningDeclaration(html, ".dash-entry-hint", "white-space")).toBe("nowrap");
    expect(winningDeclaration(html, ".dash-entry-hint", "overflow")).toBe("hidden");
    expect(winningDeclaration(html, ".dash-entry-hint", "text-overflow")).toBe("ellipsis");
    expect(winningDeclaration(html, ".dash-entry-slot", "pointer-events")).toBe("none");
    expect(winningDeclaration(html, ".dash-entry-slot", "font-size")).toBe("var(--fs-prose)");
    expect(winningDeclaration(html, ".dash-entry-slot", "line-height")).toBe("1.5");
    expect(winningDeclaration(html, ".dash-entry-slot", "min-height")).toBe("1.5em");
  });

  /* The start screen's box keeps its bright frame only while the panel holds the keys. */
  it("quiets the start screen's box without the keys, and gives it INPUT's colour with the caret in it", () => {
    const box = (focused: string) =>
      `<div class="agent-ui-root" data-pane-focused="${focused}"><div class="dash-input" aria-current="true"><div class="composer">c</div></div></div>`;
    expect(winningDeclaration(box("false"), ".composer", "border-color")).toBe("var(--rule-soft)");
    expect(winningDeclaration(box("false"), ".composer", "outline")).toBe("none");
    expect(winningDeclaration(box("true"), ".composer", "border-color")).toBe("var(--nv-fg)");
    const typing = splitRules(withoutComments).find(
      (r) => r.parent === null && r.selector === '.agent-ui-root[data-pane-focused="true"] .dash-input .composer:focus-within',
    )!;
    expect(typing.declarations).toMatch(/border-color:\s*var\(--nv-focus-ring\);/);
  });

  /* A start-screen row's title or tabs are cut by the label's ellipsis, never by the component, and its keycap
     wins over the global `kbd` rule and the user-agent's on every property that would change its look. */
  it("cuts a start-screen row's detail with CSS, and draws its keycap the same in every engine", () => {
    const row =
      '<div class="dash-item" aria-current="true"><span class="dash-label">Resume last <span class="dash-detail">t</span></span><kbd class="dash-key">r</kbd></div>';
    expect(winningDeclaration(row, ".dash-label", "text-overflow")).toBe("ellipsis");
    expect(winningDeclaration(row, ".dash-label", "white-space")).toBe("nowrap");
    expect(winningDeclaration(row, ".dash-label", "overflow")).toBe("hidden");
    expect(winningDeclaration(row, ".dash-key", "font-family")).toBe("var(--nv-font-mono)");
    // The box's keycap size, so the row keys and the box's `i` read as one kind of key.
    expect(winningDeclaration(row, ".dash-key", "font-size")).toBe("var(--fs-sm)");
    expect(winningDeclaration(row, ".dash-key", "padding")).toBe("0px 7px");
    expect(winningDeclaration(row, ".dash-key", "line-height")).toBe("1.2");
    expect(winningDeclaration(row, ".dash-key", "font-weight")).toBe("normal");
    expect(winningDeclaration(row, ".dash-key", "font-style")).toBe("normal");
    // Under the cursor the keycap keeps an edge in the muted ink, which a light scheme's cursorline cannot swallow.
    expect(winningDeclaration(row, ".dash-key", "border-color")).toBe("var(--nv-muted)");
  });

  /* A flash is one row: neither `.status-band` nor its message may wrap, and a message the layout under-counted (a
     fallback glyph wider than a cell) ends in an ellipsis instead of being clipped. Only a y/n prompt has a wrap rule. */
  it("keeps a flash on the band's one row: fixed height, no wrap, an ellipsis at the message's end", () => {
    const band = `<div class="status-band"><button class="band-open"><span class="band-seg band-message">x</span></button></div>`;
    expect(winningDeclaration(band, ".status-band", "height")).toBe("var(--nv-editor-row, 24px)");
    expect(winningDeclaration(band, ".status-band", "overflow")).toBe("hidden");
    expect(winningDeclaration(band, ".status-band", "flex-wrap")).toBeNull();
    expect(winningDeclaration(band, ".band-message", "white-space")).toBe("nowrap");
    expect(winningDeclaration(band, ".band-message", "text-overflow")).toBe("ellipsis");
    expect(winningDeclaration(band, ".band-message", "overflow")).toBe("hidden");
    expect(winningDeclaration(band, ".band-message", "min-width")).toBe("0px");
    // Nothing on the band's row ever wraps; what may not be cut goes on its own lines above it, never clipped.
    expect(withoutComments).not.toMatch(/status-band--wrap|band-wraps/);
    const above = `<div class="status-band-above"><div class="band-above-line band-message">x</div></div>`;
    expect(winningDeclaration(above, ".status-band-above", "white-space")).toBe("pre-wrap");
    expect(winningDeclaration(above, ".status-band-above", "overflow-wrap")).toBe("anywhere");
    expect(winningDeclaration(above, ".status-band-above", "overflow")).toBeNull();
    expect(winningDeclaration(above, ".status-band-above", "height")).toBeNull();
    expect(winningDeclaration(above, ".band-above-line", "overflow")).toBeNull();
    expect(winningDeclaration(above, ".band-above-line", "text-overflow")).toBeNull();
  });

  /* A layer that takes no pointer is invisible to the hit test the band's look measurement makes, so the
     measurement lists every such layer by name (`PASS_THROUGH_LAYERS`); a new one must join that list. */
  it("names every pointer-events: none layer in PASS_THROUGH_LAYERS, but the band's never-painted ruler", () => {
    const selectors: string[] = [];
    for (const m of withoutComments.matchAll(/([^{}]+)\{[^}]*pointer-events:\s*none[^}]*\}/g)) {
      for (const sel of m[1].split(",")) selectors.push(sel.trim());
    }
    expect(selectors.length).toBeGreaterThan(0);
    for (const sel of selectors) {
      if (sel === ".band-measure") continue;
      expect(PASS_THROUGH_LAYERS.some((layer) => sel === layer || sel.endsWith(` ${layer}`)), sel).toBe(true);
    }
  });

  /* The band continues the editor's statusline across the divider: the same fill, lifted
     above the command-line rows so the two lines stay aligned even with no configuration. */
  it("continues the editor's statusline: its fill and text pair, on its row above the command line", () => {
    const band = `<div class="status-band"><div class="band-open">x</div></div>`;
    expect(winningDeclaration(band, ".status-band", "background")).toBe("var(--nv-chrome)");
    expect(winningDeclaration(band, ".status-band", "color")).toBe("var(--nv-chrome-fg)");
    expect(winningDeclaration(band, ".band-open", "color")).toBe("var(--nv-chrome-fg)");
    expect(winningDeclaration(band, ".status-band", "margin-bottom")).toBe(
      "calc(var(--nv-editor-cmd-rows, 0) * var(--nv-editor-row, 24px))",
    );
    // `--nv-editor-cmd-rows` is one of the names Rust sends, only beside a measured editor.
    expect(tokensRs).toContain('"--nv-editor-cmd-rows"');
    // Manual's quiet glyph takes the chrome's muted on the band, the body's elsewhere (the chooser).
    const glyph = (inBand: boolean) =>
      inBand
        ? `<div class="status-band"><span class="mode-glyph" data-mode-name="manual">x</span></div>`
        : `<div class="chooser"><span class="mode-glyph" data-mode-name="manual">x</span></div>`;
    expect(winningDeclaration(glyph(true), ".mode-glyph", "color")).toBe("var(--nv-chrome-muted)");
    expect(winningDeclaration(glyph(false), ".mode-glyph", "color")).toBe("var(--nv-muted)");
    // Chrome-guarded text is admitted on the band because its fill is chrome.
    expect(unguardedTextColorDeclarations(".status-band .band-open { color: var(--nv-chrome-fg); }")).toEqual([]);
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

  it("fills a permission card's primary answer with the body's pair reversed, and rings it when it has focus", () => {
    const html =
      `<div class="agent-ui-root"><div class="permission-card"><div class="permission-card-buttons">` +
      `<button type="button">Approve</button><button type="button">Deny</button></div></div></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = html;
    const [approve, deny] = Array.from(document.body.querySelectorAll("button"));
    expect(getComputedStyle(approve).background).toBe("var(--nv-fg)");
    expect(getComputedStyle(approve).color).toBe("var(--nv-bg)");
    expect(getComputedStyle(deny).background).toBe("var(--nv-bg)");
    approve.focus();
    // Focus cannot fill what is already filled: the ring outside it is the mark.
    expect(winningDeclarationOn(approve, "outline")).toBe("2px solid var(--nv-fg)");
    deny.focus();
    // Deny is filled by focus like every other button, so the fill is its one focus sign: no ring on top.
    expect(winningDeclarationOn(deny, "outline")).toBe("none");
  });

  it("keeps the cursor band under a row whose control has focus: the focused control is the one solid mark", () => {
    // The conversation's cursor is a soft band, so a focused button is its only solid mark;
    // the band stays beneath it.
    const html =
      `<div class="agent-ui-root"><div class="message-list" data-focused="true"><div class="row row-permission row-current"><span class="row-sign">!</span>` +
      `<div class="row-body"><button type="button">Approve</button></div></div><div class="cursor-band" aria-hidden="true"></div></div></div>`;
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = html;
    document.body.querySelector("button")!.focus();
    expect(getComputedStyle(document.body.querySelector("button")!).background).toBe("var(--nv-fg)");
    expect(getComputedStyle(document.body.querySelector(".row-sign")!).display).toBe("none");
    const band = document.body.querySelector<HTMLElement>(".cursor-band")!;
    expect(getComputedStyle(band).display).not.toBe("none");
    expect(getComputedStyle(band).background).toBe("var(--band-fill)");
  });

  /* A HINT landing on code holds that block until the next key (`y` copies, `v` starts inside).
     Its outline uses the body's ink, thinner without the keys. The row's soft cursor band stays
     beneath it without creating a second solid mark. */
  it("outlines a HINT-landed code block, thinner without the keys, over the cursor band", () => {
    const landed = (focused: string) =>
      `<div class="message-list" data-focused="${focused}"><div class="row row-assistant row-current">` +
      `<span class="row-sign">●</span><div class="row-body"><pre class="code-block" data-hint-landed=""><code>x</code></pre></div></div>` +
      `<div class="cursor-band" aria-hidden="true"></div></div>`;
    const block = computed(landed("true"), "pre.code-block");
    expect(block.outlineStyle).toBe("solid");
    expect(block.outlineWidth).toBe("2px");
    expect(block.outlineColor).toBe("var(--nv-fg)");
    expect(computed(landed("false"), "pre.code-block").outlineWidth).toBe("1.5px");
    // The band stays under the row `j`/`k` continue from (a soft fill, not a second solid mark).
    expect(computed(landed("true"), ".cursor-band").background).toBe("var(--band-fill)");
    // Not landed: no outline.
    const plain =
      `<div class="message-list" data-focused="true"><div class="row row-assistant row-current">` +
      `<span class="row-sign">●</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    expect(computed(plain, "pre.code-block").outlineStyle).not.toBe("solid");
  });

  it("draws the conversation's cursor as a flat full-bleed band with the keys and a weaker one without", () => {
    const html = (attrs = "") => `<div class="message-list" ${attrs}><div class="cursor-band"></div></div>`;
    expect(computed(html(), ".cursor-band").background).toBe("var(--band-fill)");
    // The fills are the Rust-guarded colours (`cursor_band`, `cursor_band_inactive`), not a mix made here: a
    // plain 24%/12% mix of cursorline into bg measured 1.02-1.06:1, the only cursor mark being nearly invisible.
    expect(ROOT_TOKENS.get("--band-fill")).toBe("var(--nv-cursor-band)");
    expect(ROOT_TOKENS.get("--band-fill-inactive")).toBe("var(--nv-cursor-band-inactive)");
    expect(computed(html('data-focused="false"'), ".cursor-band").background).toBe("var(--band-fill-inactive)");
    expect(computed(html(), ".cursor-band").pointerEvents).toBe("none");
    expect(computed(html(), ".cursor-band").borderRadius).toBe("8px");
    expect(computed(html('data-visual="caret"'), ".cursor-band").display).toBe("none");
  });

  /** The conversation's cursor is a soft flat fill and nothing else: no bar at its left edge, no frame, no
   *  mode colour. The fill is carried a little above and below the row by the band's box-shadow, which is
   *  the one shadow allowed, and only in the band's own fill variables. */
  const bandMarks = (sheet: string): string[] => {
    const found: string[] = [];
    for (const rule of splitRules(sheet)) {
      const branches = rule.selector.split(",").map((b) => b.trim()).filter((b) => b.includes(".cursor-band"));
      if (branches.length === 0) continue;
      for (const branch of branches) {
        if (/(?:::?before|::?after)\s*$/.test(branch)) found.push(`${branch} is a pseudo-element mark`);
      }
      if (/(?:^|;)\s*(?:border(?!-radius)[\w-]*|outline[\w-]*)\s*:|--nv-mode-/.test(rule.declarations)) found.push(`${rule.selector} draws a frame or a mode colour`);
      const shadow = /box-shadow:\s*([^;]+);/.exec(rule.declarations)?.[1];
      if (shadow !== undefined && !/^(?:0 -?8px 0 var\(--band-fill(?:-inactive)?\),?\s*)+$/.test(shadow.trim())) {
        found.push(`${rule.selector} has a shadow other than the fill extension: ${shadow}`);
      }
    }
    return found;
  };
  it("draws the conversation's cursor with no bar, frame or mode colour on the band", () => {
    expect(bandMarks(withoutComments)).toEqual([]);
    expect(splitRules(withoutComments).some((r) => /\.cursor-band\s*::?(?:before|after)/.test(r.selector))).toBe(false);
    const row = `<div class="message-list" data-focused="true"><div class="row row-tool row-current" data-sign="✓"><span class="row-sign">✓</span><div class="row-body">x</div></div><div class="cursor-band" aria-hidden="true"></div></div>`;
    // The fill is the band behind the rows, not the conversation row's own, and no sign cell is drawn.
    expect(computed(row, ".row").background).not.toBe("var(--nv-cursorline)");
    expect(computed(row, ".row-sign").display).toBe("none");
    // Negative controls: a bar drawn with `::before` / `::after`, a border, an outline, a mode colour and a
    // foreign shadow are each caught.
    for (const extra of [
      '.cursor-band::before { content: ""; width: 3px; background: var(--nv-chrome-accent); }',
      '.message-list[data-focused="false"] .cursor-band::after { content: ""; background: var(--rule-frame); }',
      ".cursor-band { border-left: 3px solid var(--nv-fg); }",
      ".cursor-band { outline: 1px solid var(--nv-fg); }",
      ".cursor-band { background: var(--nv-mode-insert); }",
      ".cursor-band { box-shadow: -3px 0 0 var(--nv-fg); }",
    ]) {
      expect(bandMarks(`${withoutComments}\n${extra}`), extra).not.toEqual([]);
    }
  });
  it("dims an unfocused mode block even in INPUT, and draws it with no bar at its edge", () => {
    // The mode block carries no left bar in either mode (the mode is said by its label), and an
    // unfocused block is muted whichever mode it is in (the band's own muted: it sits on chrome).
    const block = computed(UNFOCUSED_INPUT_BLOCK, ".band-mode");
    expect(block.color).toBe("var(--nv-chrome-muted)");
    // A focused block inherits the bright label colour.
    const focused = computed(MODE_BLOCK.replace('data-mode="input"', 'data-mode="input" data-focused="true"'), ".band-mode");
    expect(focused.color).not.toBe("var(--nv-chrome-muted)");
    // Negative control: a per-mode rule as specific as the unfocused one, re-declared after it, wins the colour back.
    const clobbered = computed(UNFOCUSED_INPUT_BLOCK, ".band-mode", '.status-band .band-mode[data-mode="input"] { color: var(--nv-fg); }');
    expect(clobbered.color).toBe("var(--nv-fg)");
  });

  /** The chooser's solid cursor stays in its sign cell. A long title must stay beside the sign
   *  on its own line, clipped rather than wrapping below the sign column. */
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

  /** Stop is words in the activity line, without WebKit's boxed-button face; focused, it uses
   *  the same solid cursor as every focused control. */
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

  /* A focused button is drawn as the solid cursor, the fill being its one focus sign. A button already
     drawn with that fill therefore shows nothing on focus unless its own :focus rule adds a ring. */
  it("gives every button already drawn with the solid cursor fill a focus rule that changes something", () => {
    const unmarkedFocus = (css: string): string[] => {
      const rules = splitRules(css);
      const ringed = (branch: string): boolean =>
        rules.some(
          (r) =>
            r.selector
              .split(",")
              .map((b) => b.trim())
              .some((b) => b === `${branch}:focus` || b === `.agent-ui-root ${branch}:focus`) &&
            /(?:^|;)\s*(?:outline|box-shadow)\s*:\s*(?!none\b|0\b)\S/.test(r.declarations),
        );
      const missing: string[] = [];
      for (const rule of rules) {
        if (!/(?:^|;)\s*background(?:-color)?:\s*var\(--nv-fg\)\s*(?:;|$)/.test(rule.declarations)) continue;
        for (const branch of rule.selector.split(",").map((b) => b.trim())) {
          // The focus rules themselves, CARET's selection and the sign cells are not buttons.
          if (/:focus|::selection|-sign$/.test(branch)) continue;
          if (!ringed(branch)) missing.push(branch);
        }
      }
      return missing;
    };
    expect(unmarkedFocus(withoutComments)).toEqual([]);
    // The check sees what it claims to: a filled button with no ring, or only an outline switched off, is caught.
    expect(unmarkedFocus(".agent-ui-root .go { background: var(--nv-fg); }")).toEqual([".agent-ui-root .go"]);
    expect(
      unmarkedFocus(".go { background: var(--nv-fg); } .agent-ui-root .go:focus { outline: none; }"),
    ).toEqual([".go"]);
    expect(
      unmarkedFocus(".go { background: var(--nv-fg); } .agent-ui-root .go:focus { outline: 2px solid var(--nv-fg); }"),
    ).toEqual([]);
    // The band's jump button is among those the scan reaches.
    expect(withoutComments).toMatch(/\.band-unread\s*\{[^}]*background:\s*var\(--nv-fg\)/);
  });

  /* A lualine mode block's fill is the editor's own colour and unguarded; with no lualine it is the cursorline
     stand-in, 1.15:1 on bg. Drawn as the composer's ring it can be fainter than the unfocused frame, so rings,
     borders and outlines take `--nv-focus-ring` (guarded at 3:1 in Rust) and the mode fills stay fills. */
  it("never draws a status-block colour as a border, outline or shadow", () => {
    const stroke = /^(?:border(?:-(?:top|right|bottom|left|inline|block)(?:-[a-z]+)?)?(?:-color)?|outline(?:-color)?|box-shadow|text-decoration(?:-color)?|caret-color|accent-color)$/i;
    const statusStrokes = (css: string): string[] => {
      const found: string[] = [];
      for (const rule of splitRules(css)) {
        for (const raw of rule.declarations.split(";")) {
          const m = raw.trim().match(/^([a-z-]+)\s*:\s*(.*)$/is);
          if (m !== null && stroke.test(m[1]) && /var\(--nv-status-(?:normal|insert|visual)-bg\)/.test(m[2])) {
            found.push(`${rule.selector} { ${raw.trim()} }`);
          }
        }
      }
      return found;
    };
    expect(statusStrokes(withoutComments)).toEqual([]);
    // The guard sees what it claims to: a ring or a border written in a mode fill is caught, a fill is not.
    expect(statusStrokes(".composer:focus-within { outline: 1px solid var(--nv-status-insert-bg); }")).toHaveLength(1);
    expect(statusStrokes(".composer { border-color: var(--nv-status-visual-bg); }")).toHaveLength(1);
    expect(statusStrokes(".mode-block { background: var(--nv-status-normal-bg); }")).toEqual([]);
    // The two focus rules draw the guarded ring.
    const ring = splitRules(withoutComments).filter((r) => r.selector.endsWith(".composer:focus-within"));
    expect(ring.map((r) => r.selector).sort()).toEqual([
      '.agent-ui-root[data-pane-focused="true"] .dash-input .composer:focus-within',
      ".composer:focus-within",
    ]);
    for (const r of ring) expect(r.declarations).toContain("var(--nv-focus-ring)");
  });

  /* No tint on the composer: the page in a hairline frame,
     rounded a little more than the conversation's blocks, with no prompt cell before the text. */
  it("draws the composer as a framed box on the page with a prompt cell, and a visible ring over it while it has the keys", () => {
    const rules = splitRules(withoutComments);
    const box = rules.find((r) => r.selector === ".composer")!;
    expect(box.declarations).toMatch(/border-radius:\s*14px;/);
    expect(box.declarations).toMatch(/background:\s*var\(--nv-bg\);/);
    expect(box.declarations).toMatch(/border:\s*1px solid var\(--rule-frame\);/);
    expect(rules.find((r) => r.selector === ".composer::before")).toBeUndefined();
    const focus = rules.find((r) => r.selector === ".composer:focus-within")!;
    expect(focus.declarations).toMatch(/outline:\s*1px solid var\(--nv-focus-ring\);/);
    expect(focus.declarations).toMatch(/border-color:\s*var\(--nv-focus-ring\);/);
  });
  /** The composer has one frame: neither BROWSE's stand-in nor INPUT's textarea draws another,
   *  including WebKit's focus ring. */
  it("draws no box or focus ring of its own inside the composer card, in either mode", () => {
    const hint = computed(`<div class="composer"><div class="composer-browse-hint">x</div></div>`, ".composer-browse-hint");
    for (const side of ["Top", "Right", "Bottom", "Left"] as const) {
      expect(hint[`border${side}Style` as "borderTopStyle"], side).toMatch(/^(none|)$/);
    }
    expect(hint.borderRadius).toMatch(/^(0|0px|)$/);
    const box = computed(`<div class="composer"><textarea></textarea></div>`, ".composer textarea");
    expect(box.outlineStyle).toBe("none");
  });

  /* Pressing `i` swaps BROWSE's stand-in line for the textarea; the empty composer must be the same height
     in both, or the conversation above it moves. One line height and one min-height, and the keycap stays
     inside that line box. */
  it("gives the empty composer one line height and one min-height in BROWSE and INSERT", () => {
    const browse = `<div class="composer"><div class="composer-browse-hint">Ask the agent...<span class="composer-key">i</span></div></div>`;
    const input = `<div class="composer"><textarea rows="1"></textarea></div>`;
    const hint = computed(browse, ".composer-browse-hint");
    const area = computed(input, ".composer textarea");
    expect(area.lineHeight).toBe("1.6");
    expect(hint.lineHeight).toBe(area.lineHeight);
    // jsdom resolves an `em` length against the font size, so the written value is read from the rules.
    expect(hint.minHeight).toBe(area.minHeight);
    for (const selector of [".composer-browse-hint", ".composer textarea"]) {
      expect(splitRules(withoutComments).find((r) => r.selector === selector)!.declarations, selector).toMatch(
        /min-height:\s*1\.6em;/,
      );
    }
    expect(hint.fontSize).toBe(area.fontSize);
    // The keycap's line box is smaller than the stand-in's, so its border and padding cannot grow the line.
    expect(computed(browse, ".composer-key").lineHeight).toBe("1.2");
    // Negative control: a stand-in left at the inherited line height would differ from the textarea.
    const stripped = css.replace(/(\.composer-browse-hint \{[^}]*?)line-height: 1\.6;/, "$1");
    expect(stripped).not.toBe(css);
    document.head.innerHTML = `<style>${stripped}</style>`;
    document.body.innerHTML = browse;
    expect(getComputedStyle(document.body.querySelector(".composer-browse-hint")!).lineHeight).not.toBe("1.6");
  });

  /* A flex item's leading whitespace is removed, so the space before the hint's dash cannot
     separate it from the preceding text. jsdom has no layout; pin the rule that supplies the gap. */
  /* BROWSE's stand-in ends in the key alone at the right edge; a queued count precedes it. */
  it("puts BROWSE's key at the box's right end, after a queued count when there is one", () => {
    const html = (queued: boolean) =>
      `<div class="composer"><div class="composer-browse-hint">Ask the agent...${queued ? '<span class="composer-queued">+2 queued</span>' : ""}<span class="composer-key">i</span></div></div>`;
    expect(winningDeclaration(html(false), ".composer-key", "margin-left")).toBe("auto");
    expect(winningDeclaration(html(false), ".composer-key", "color")).toBe("var(--nv-muted)");
    expect(winningDeclaration(html(true), ".composer-key", "margin-left")).toMatch(/^0(px)?$/);
    expect(winningDeclaration(html(true), ".composer-queued", "margin-left")).toBe("auto");
  });

  /* Preserve line breaks in the missing-sidecar remedy so its paths and instructions stay separate. */
  it("keeps a problem remedy's own line breaks", () => {
    const remedy = computed(`<div class="row-problem"><div class="row-problem-remedy">a\nb</div></div>`, ".row-problem-remedy");
    expect(remedy.whiteSpace).toMatch(/^pre(-line|-wrap)?$/);
  });

  it("gives the composer the panel's own font and caps its growth", () => {
    const box = computed(`<div class="composer"><textarea></textarea></div>`, ".composer textarea");
    expect(box.fontFamily).not.toMatch(/-webkit-small-control/);
    // jsdom resolves `30vh` against `window.innerHeight` rather than reporting the token verbatim
    // (unlike a `var()`, which every other test in this file checks unresolved). Use a numeric
    // comparison with `parseFloat`, rather than a string
    // comparison against `window.innerHeight * 0.3`: the two float multiplications round
    // differently (`307.2` here vs `307.20000000000005` in plain JS arithmetic).
    expect(box.maxHeight.endsWith("px")).toBe(true);
    expect(Number.parseFloat(box.maxHeight)).toBeCloseTo(window.innerHeight * 0.3, 5);
    expect(box.overflowY).toBe("auto");
    const reason = computed(`<div class="permission-card"><input type="text"></div>`, ".permission-card input");
    expect(reason.lineHeight).toBe("1.4");
  });

  /** At about 348px, content-box `width: 100%` plus 48px padding overflows the empty tab sideways. */
  it("keeps the empty tab inside its column", () => {
    const tab = computed(`<div class="agent-ui-root"><div class="empty-tab">x</div></div>`, ".empty-tab");
    expect(tab.boxSizing).toBe("border-box");
    expect(tab.minWidth).toBe("0px");
    const title = computed(`<div class="empty-tab-resume"><strong class="session-title">t</strong></div>`, ".session-title");
    expect(title.overflowWrap).toBe("anywhere");
  });
});

/** Every theme variable the stylesheet reads must be emitted by Rust. An unknown variable
 *  silently falls back to the property's initial value, which shape-only checks cannot detect.
 *  Derive the allowed set from the three emission sources in `core/src/theme/tokens.rs` rather
 *  than copying a list that can drift. The spelling-sensitive parse checks its own sentinels,
 *  so refactors fail loudly rather than silently admitting every name. */
/* Long Bash commands, unbroken tool fields, prose URLs and tables must not push the whole
   conversation sideways. A tool's `<pre>` otherwise keeps the UA's `white-space: pre`.
   jsdom has no layout: require each line to break where needed and each rendered `<pre>` to wrap
   or scroll within its own box. `shell/tests/panel_stream_scroll.rs` checks the real engine. */
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
      // Bash uses `.permission-card-command`; an unknown tool keeps raw JSON input in the fixture.
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
    // Negative control: removing the command's wrapping rule exposes sideways overflow.
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

  it("tool_output_previews_are_mono_at_the_code_size", () => {
    const line = foldedPreview().querySelector<HTMLElement>(".tool-result-preview-line")!;
    expect(winningDeclarationOn(line, "font-family")).toMatch(/^var\(--nv-font-mono\b/);
    expect(winningDeclarationOn(line, "font-size")).toBe("var(--fs-code)");
  });

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

  it("wraps, whole and in full-strength ink, the one line a refused call's folded failure shows (the CLI's own reason)", () => {
    const reason = "Permission for this action was denied by the Claude Code auto mode classifier. Reason: " + "[Git Destructive] ".repeat(30);
    const folded = renderToolCall(
      {
        seq: 1,
        toolUseId: "t1",
        name: "Bash",
        input: { command: "git push --force origin main" },
        result: { content: reason, isError: true },
        denied: { reasonType: "classifier", reason: "[Git Destructive]" },
      },
      false,
    );
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML =
      `<div class="message-list"><div class="row"><span class="row-sign"></span><div class="row-body">` +
      renderToStaticMarkup(createElement("div", null, folded)) +
      `</div></div></div>`;
    const line = document.querySelector<HTMLElement>(".tool-result-preview-line")!;
    expect(line.textContent).toBe(reason);
    expect(winningDeclarationOn(line, "white-space")).toBe("pre-wrap");
    expect(winningDeclarationOn(line, "overflow")).toBe("visible");
    expect(winningDeclarationOn(line, "text-overflow")).toBe("clip");
    // The ordinary result's ink, guarded against the page, which the line inherits from the preview.
    expect(winningDeclarationOn(line.parentElement!, "color")).toBe("var(--nv-muted)");
    expect(winningDeclarationOn(line, "color")).toBeFalsy();
  });

  it("holds the preview's lines in one zero-minimum track beside the connector, which is what keeps a long line from widening the row", () => {
    const preview = foldedPreview();
    expect(winningDeclarationOn(preview, "display")).toBe("grid");
    expect(winningDeclarationOn(preview, "grid-template-columns")).toBe("var(--sign-w) minmax(0, 1fr)");
    // The lines sit in the second track; the `⎿` is the first, generated content.
    expect(winningDeclarationOn(preview.querySelector(".tool-result-preview-line")!, "grid-column")).toBe("2");
  });

  it("draws a call's output, a failure's too, under a muted connector with no fill, no frame and no rail", () => {
    const ok = foldedPreview();
    // On `--nv-bg`, so the body's muted ink, and the count is muted too.
    expect(winningDeclarationOn(ok, "color")).toBe("var(--nv-muted)");
    expect(winningDeclarationOn(ok.querySelector(".tool-result-preview-more")!, "color")).toBe("var(--nv-muted)");
    const failed = foldedPreview("", true);
    expect(failed.getAttribute("data-state")).toBe("error");
    // A failure takes nothing of its own: the track, the ink and the count are the ordinary result's.
    for (const property of ["border", "border-left", "background", "padding", "border-radius", "width", "min-width", "max-width"]) {
      expect(winningDeclarationOn(failed, property), property).toBeFalsy();
    }
    expect(winningDeclarationOn(failed, "color")).toBe("var(--nv-muted)");
    expect(winningDeclarationOn(failed, "grid-template-columns")).toBe("var(--sign-w) minmax(0, 1fr)");
    expect(winningDeclarationOn(failed.querySelector(".tool-result-preview-line")!, "grid-column")).toBe("2");
    expect(winningDeclarationOn(failed.querySelector(".tool-result-preview-more")!, "color")).toBe("var(--nv-muted)");
    // The connector is generated content in the first track, muted, and never read out.
    const connector = splitRules(withoutComments).find((r) => r.selector === ".tool-result::before,\n.tool-result-preview::before" || r.selector.replace(/\s+/g, " ") === ".tool-result::before, .tool-result-preview::before");
    expect(connector?.declarations).toMatch(/content:\s*"⎿" \/ "";/);
    expect(connector?.declarations).toMatch(/color:\s*var\(--nv-muted\);/);
    // Coloured side rails belong only to start-screen cards and the trust prompt, never the
    // conversation or foot.
    const railed = splitRules(withoutComments)
      .filter((r) => /border-left:\s*\d+px solid var\(--(?:nv-warn|nv-error|rule-soft)\)/.test(r.declarations))
      .flatMap((r) => r.selector.split(",").map((b) => b.trim()));
    expect(railed.sort()).toEqual([".handoff-card .warning", ".handoff-confirm .warning", ".trust-note", ".trust-outside"]);
  });

  it("scrolls a markdown table in its own box and keeps its words whole", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = `<div class="message-list"><div class="row"><div class="row-body">${renderMarkdown("| a | b |\n|---|---|\n| supercalifragilistic | 2 |")}</div></div></div>`;
    const wrapper = document.querySelector<HTMLElement>(".table-scroll")!;
    expect(winningDeclarationOn(wrapper, "overflow-x")).toBe("auto");
    const cell = document.querySelector<HTMLElement>(".table-scroll td")!;
    expect(winningDeclarationOn(cell, "overflow-wrap")).toBe("normal");
  });
});

/* The `?` overlay must fit 520px and a 348px panel beside a Lua pane. A nowrap keys column,
   especially `Ctrl+0 / Ctrl+Keypad0 / Ctrl+Keypad0 (NumLock off)`, can push descriptions off-screen.
   Keys may break at spaces; individual key names stay whole because no overflow-wrap is set. */
describe("index.css: the ? overlay never scrolls sideways", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  function keyCells(extraCss = ""): HTMLElement[] {
    const long = { keys: "Ctrl+0 / Ctrl+Keypad0 / Ctrl+Keypad0 (NumLock off)", what: "Text size reset (every module)" };
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
    // Negative control: nowrap keys must be detected.
    expect(winningDeclarationOn(keyCells(".keymap-overlay td:first-child { white-space: nowrap; }")[0], "white-space")).toBe(
      "nowrap",
    );
  });
});

describe("--nv-* names", () => {
  /** What `ThemeTokens::css_vars` puts on the document, reconstructed from `tokens.rs`'s source. */
  const emitted = new Set<string>([
    // 1. the `colours` array in `css_vars`: `("chrome-fg", self.chrome_fg)` -> `--nv-chrome-fg`, and a pair's
    //    half, `("status-normal-bg", self.status_normal.0)`.
    ...Array.from(tokensRs.matchAll(/\("([a-z-]+)",\s*self\.[a-z_]+(?:\.\d)?\)/g), (m) => `--nv-${m[1]}`),
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

/** WebKitGTK's horizontal scrollbar covers the lower 21px of a 41px tab bar when three tabs
 *  overflow a 520px panel, intercepting clicks there. Hide only the scrollbar: the active tab
 *  still scrolls into view and `h`/`l` still move, as on tmux's status line. */
it("draws no scrollbar on the tab bar, so a click anywhere on a tab reaches it", () => {
  const sheet = stripComments(css);
  const bar = rulesMatching(sheet, ".tab-bar").find((r) => r.selector.trim() === ".tab-bar");
  expect(bar?.body).toMatch(/overflow-x:\s*auto/);
  expect(bar?.body).toMatch(/scrollbar-width:\s*none/);
  const webkit = rulesMatching(sheet, ".tab-bar::-webkit-scrollbar");
  expect(webkit.map((r) => r.body).join(";")).toMatch(/display:\s*none/);
});

/** Surface text needs its own guarded pair. With zaibatsu's NormalFloat bg equal to Normal's
 *  fg, the body's pair can be white on white. Render the real boxes, find each text's nearest
 *  fill, and require surface-guarded text or the deliberate syntax-colour exemption. Compare
 *  declarations, not pixels: jsdom inherits `color` but does not resolve `var()`. */
describe("text on a --nv-surface fill", () => {
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
      // A state glyph on a surface (an edit head's `+`/`−`) keeps its own one colour (`GLYPH_COLOURS`).
      const glyph = Object.entries(GLYPH_COLOURS).some(([sel, c]) => !sel.includes("::") && el.matches(sel) && color === `var(${c})`);
      if (!glyph && !/^var\(--nv-(surface-fg|surface-muted|syn-[a-z]+)\)$/.test(color)) {
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

/** Rust mixes three tints and guards a text pair against each. Body-muted text measures
 *  3.6-4.8:1 on them, a dimmed composer hint 2.3-3.0:1, and WebKit's darkGray placeholder about
 *  1.9:1 on a light tint. Render the real boxes, find each text's nearest fill, and require its
 *  matching tint pair -- and refuse that pair elsewhere. */
describe("text on a tinted fill", () => {
  afterEach(() => {
    document.head.innerHTML = "";
    document.body.innerHTML = "";
  });

  const PAIRS: Record<string, RegExp> = {
    "var(--nv-tint)": /^var\(--nv-tint-(fg|muted)\)$/,
    "var(--nv-warn-tint)": /^var\(--nv-warn-tint-(fg|muted)\)$/,
    "var(--nv-error-tint)": /^var\(--nv-error-tint-(fg|muted)\)$/,
  };

  /** The nearest fill behind `el` as an `--nv-*` token, a `:root` alias such as `--tint-you` resolved
   *  through the stylesheet's own declarations. `null` for the body. */
  function nearestFill(el: Element | null): string | null {
    for (let node = el; node !== null; node = node.parentElement) {
      const style = getComputedStyle(node);
      for (const raw of [style.background, style.backgroundColor]) {
        const alias = (raw ?? "").match(/^var\((--[\w-]+)\)$/);
        const value = alias && !alias[1].startsWith("--nv-") ? (ROOT_TOKENS.get(alias[1]) ?? raw) : raw;
        const token = (value ?? "").match(/var\(--nv-[a-z-]+\)/);
        if (token) return token[0];
      }
    }
    return null;
  }

  function tintFixtures(): string {
    const call = (seq: number, name: string, input: unknown, content: string, detailed: boolean, denied?: ToolCallRecord["denied"]) =>
      renderToStaticMarkup(
        createElement(
          "div",
          null,
          renderToolCall({ seq, toolUseId: `t${seq}`, name, input, result: { content, isError: true }, ...(denied ? { denied } : {}) }, detailed),
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
    const composer = (props: Record<string, unknown>) =>
      renderToStaticMarkup(
        createElement(Composer, {
          disabled: false,
          sessionEnded: false,
          closing: false,
          restoredDraft: null,
          mode: "browse",
          onModeChange: () => {},
          onSend: () => {},
          running: false,
          ...props,
        } as Parameters<typeof Composer>[0]),
      );
    const rows = [
      `<div class="row row-prompt"><div class="row-body">why does the view move?</div></div>`,
      `<div class="row row-assistant"><div class="row-body"><div>${renderMarkdown("Run `cargo test` and see [`main.rs`](main.rs).")}</div></div></div>`,
      `<div class="row row-tool"><div class="row-body">${call(1, "Bash", { command: "false" }, "line one\nline two\nline three\nline four", false)}</div></div>`,
      `<div class="row row-tool"><div class="row-body">${call(2, "Bash", { command: "false" }, "x\n".repeat(4000), true)}</div></div>`,
      `<div class="row row-tool"><div class="row-body">${call(3, "Bash", { command: "git push --force" }, "Permission for this action was denied.", false, { reasonType: "classifier", reason: "[Git Destructive]" })}</div></div>`,
      card(7, "Bash", { command: "cargo test", description: "Run the tests" }),
      card(8, "Edit", { file_path: "/p/a.rs", old_string: "a\n", new_string: "b\n" }),
      card(9, "Write", { file_path: "/p/b.rs", content: "x\n" }),
      card(10, "mcp__demo__lookup", { k: "v" }),
      card(11, "Write", { file_path: "/p/.git/config", content: "x\n" }, {
        providerPrompt: {
          reason: "writes inside .git",
          description: "The agent wants to write a file it may not",
          blockedPath: null,
          matchedAskRule: null,
          unrecognizedOrigin: null,
        },
      }),
      card(12, "Bash", { command: "ls" }, { sessionEnded: true }),
    ];
    const lost = renderToStaticMarkup(
      createElement(Row, {
        kind: "error",
        sign: "✗",
        role: "alert",
        problem: { headline: "The sidecar stopped.", remedy: "Looked for it at:\n/usr/lib/eitri/sidecar" },
        children: [
          createElement("strong", { key: "s" }, "This session was lost."),
          createElement("pre", { key: "p" }, "transport error: broken pipe"),
          createElement("div", { key: "h", className: "row-hint" }, "Press r to resume this conversation."),
        ],
      }),
    );
    return (
      `<div class="agent-ui-root"><div class="agent-ui-conversation"><div class="message-list">` +
      rows.join("") +
      `</div>${lost}<div class="composer-slot">` +
      composer({ closing: true, restoredDraft: null }) +
      composer({ mode: "browse", queueCount: 2 }) +
      `</div></div></div>`
    );
  }

  /** Every element holding text of its own that is drawn at all (a hidden sign cell is not). */
  function textElements(): Element[] {
    const shown = (el: Element | null): boolean =>
      el === null || (getComputedStyle(el).display !== "none" && shown(el.parentElement));
    return Array.from(document.body.querySelectorAll("*")).filter(
      (el) =>
        shown(el) &&
        Array.from(el.childNodes)
          .filter((n) => n.nodeType === Node.TEXT_NODE)
          .map((n) => n.textContent ?? "")
          .join("")
          .trim() !== "",
    );
  }

  it("draws every text on a tint in the pair Rust guards against that tint", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = tintFixtures();
    const offenders: string[] = [];
    const seen: Record<string, number> = {};
    for (const el of textElements()) {
      const fill = nearestFill(el);
      if (fill === null || PAIRS[fill] === undefined) continue;
      seen[fill] = (seen[fill] ?? 0) + 1;
      const color = getComputedStyle(el).color;
      // Syntax colours in a diff keep their deliberate non-guarantee, as on a code block, and the `+`/`-`
      // glyphs of an edit's counts and a diff's gutter are glyphs in `GLYPH_COLOURS`, held to the non-text
      // threshold and not to a text pair.
      const glyph = el.matches(".edit-sign, .diff-gutter") && /^var\(--nv-(ok|error)\)$/.test(color);
      if (!PAIRS[fill].test(color) && !/^var\(--nv-syn-[a-z]+\)$/.test(color) && !glyph) {
        offenders.push(`${fill}: <${el.tagName.toLowerCase()} class="${el.className}"> "${(el.textContent ?? "").trim().slice(0, 24)}": ${color || "(none)"}`);
      }
    }
    // The fixture really put text on each tint.
    // Your messages and inline code are on this tint; the composer is off it.
    expect(seen["var(--nv-tint)"] ?? 0, "your own tint").toBeGreaterThan(2);
    expect(seen["var(--nv-warn-tint)"] ?? 0, "the warn tint").toBeGreaterThan(8);
    // A failed call's output and the lost-session card are on this tint.
    expect(seen["var(--nv-error-tint)"] ?? 0, "the error tint").toBeGreaterThan(2);
    expect(offenders).toEqual([]);
  });

  it("never draws a tint's pair on any other fill", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = tintFixtures();
    const offenders: string[] = [];
    let off = 0;
    for (const el of textElements()) {
      const fill = nearestFill(el);
      const color = getComputedStyle(el).color;
      const tinted = /^var\(--nv-(warn-|error-)?tint-(fg|muted)\)$/.test(color);
      if (fill !== null && PAIRS[fill] !== undefined) continue;
      off += 1;
      if (tinted) offenders.push(`${fill ?? "bg"}: <${el.tagName.toLowerCase()} class="${el.className}">: ${color}`);
    }
    expect(off, "the fixture must also hold text off the tints").toBeGreaterThan(5);
    expect(offenders).toEqual([]);
  });

  it("detects a body-pair text on a tint (negative control)", () => {
    document.head.innerHTML = `<style>${css} .permission-card-description { color: var(--nv-muted); }</style>`;
    document.body.innerHTML = tintFixtures();
    const description = document.querySelector(".permission-card .permission-card-description")!;
    expect(nearestFill(description)).toBe("var(--nv-warn-tint)");
    expect(PAIRS["var(--nv-warn-tint)"].test(getComputedStyle(description).color)).toBe(false);
  });

  it("draws the composer's placeholder in the body's muted ink at full strength, and its key undimmed", () => {
    // The composer is on the body fill, so it takes the body's guarded pair.
    const rules = splitRules(withoutComments);
    const placeholder = rules.find((r) => r.parent === null && r.selector === ".composer textarea::placeholder");
    expect(placeholder, "WebKit's UA placeholder colour is darkGray, whatever the card's").toBeDefined();
    expect(placeholder!.declarations).toMatch(/(?:^|;)\s*color:\s*var\(--nv-muted\);/);
    expect(placeholder!.declarations).toMatch(/(?:^|;)\s*opacity:\s*1;/);
    const key = rules.filter((r) => r.selector.split(",").some((b) => b.trim().startsWith(".composer-key")));
    expect(key.length).toBeGreaterThan(0);
    for (const rule of key) expect(rule.declarations).not.toMatch(/opacity/);
  });
});

describe("VISUAL mode", () => {
  it("index.css's user-select: none selectors equal visual.ts's own VISUAL_CHROME, exactly", () => {
    const rules = splitRules(stripComments(css));
    const found = new Set<string>();
    for (const rule of rules) {
      if (!/user-select\s*:\s*none\s*;?/.test(rule.declarations)) continue;
      for (const selector of rule.selector.split(",")) found.add(selector.trim());
    }
    expect(found.has(".diagram-error")).toBe(true);
    expect(found).toEqual(new Set(VISUAL_CHROME));
  });

  it("hides every VISUAL_CHROME member under data-visual-copying, the rule copySelectionText's attribute actually needed", () => {
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
    expect(found.has(".diagram-error")).toBe(true);
    expect(found).toEqual(new Set(VISUAL_CHROME));
  });

  it("highlights a selection in nvim's Visual colour while VISUAL/V-LINE is on, and turns off the cursorline fill where the two coincide", () => {
    // Strip comments first: a selector name mentioned in prose can fool `rulesMatching`'s
    // substring search into finding the wrong captured rule.
    const stripped = stripComments(css);
    const selectionRule = rulesMatching(stripped, "::selection").find((r) => r.selector.includes("[data-visual]"));
    expect(selectionRule, "no .message-list[data-visual] ::selection rule").toBeDefined();
    expect(selectionRule!.body).toContain("background: color-mix(in srgb, var(--nv-cursorline) 99%, transparent)");
    // Text keeps its own colour: this rule must not also set `color`.
    expect(selectionRule!.body).not.toMatch(/(?<!background-)color\s*:/);
    const rowCurrentOverride = rulesMatching(stripped, ".row-current").find((r) => r.selector.includes("[data-visual]"));
    expect(rowCurrentOverride, "no .message-list[data-visual] .row-current rule").toBeDefined();
    expect(rowCurrentOverride!.body).toContain("background: none");
  });

  /* WebKit paints an opaque `::selection` background as a translucent colour that matches it only over
     white, so on a dark scheme the selection lands within a shade of `bg`: measured in the sandbox,
     nvim's default Visual `#4f5258` was drawn `#10141b` over its `#14161b` and CARET's `--nv-fg` block
     came out grey. A fill that is already translucent is painted as given. */
  it("every selection fill in the chat is translucent, so WebKit paints the token and not a shade of the background", () => {
    const rules = splitRules(stripComments(css)).filter((r) => r.selector.includes("::selection"));
    expect(rules.map((r) => r.selector).sort()).toEqual([
      '.message-list[data-visual="caret"] ::selection',
      '.message-list[data-visual="visual"] ::selection, .message-list[data-visual="vline"] ::selection',
      ".message-list[data-visual] ::selection",
    ]);
    // VISUAL and V-LINE draw their own selection (`visualPaint.ts`): the engine's is not painted at all.
    const own = rules.find((r) => r.selector.includes('"vline"'))!;
    expect(own.declarations).toMatch(/background:\s*transparent/);
    for (const rule of rules.filter((r) => r !== own)) {
      const fills = rule.declarations
        .split(";")
        .map((d) => d.trim())
        .filter((d) => /^background(?:-color)?\s*:/.test(d));
      expect(fills, `${rule.selector} sets one fill`).toHaveLength(1);
      expect(fills[0], `${rule.selector}'s fill must not be an opaque colour`).toMatch(SELECTION_FILL);
    }
    // The pattern refuses what WebKit would repaint: a bare token, and a mix that comes out opaque.
    expect("background: var(--nv-cursorline)").not.toMatch(SELECTION_FILL);
    expect("background: color-mix(in srgb, var(--nv-cursorline) 100%, transparent)").not.toMatch(SELECTION_FILL);
    expect("background: color-mix(in srgb, var(--nv-cursorline) 99%, var(--nv-bg))").not.toMatch(SELECTION_FILL);
  });
});

describe("VISUAL's own selection layer", () => {
  it("lies over the rows, blended so the text keeps its colour, and shows only in VISUAL and V-LINE", () => {
    const rules = splitRules(stripComments(css));
    const layer = rules.find((r) => r.selector === ".visual-paint")!;
    expect(layer.declarations).toMatch(/position:\s*absolute/);
    expect(layer.declarations).toMatch(/pointer-events:\s*none/);
    // No stacking context between the boxes and the conversation: with a z-index on the layer the boxes would blend
    // with an empty group of their own and cover the text, and a blend on the layer drew them opaque in WebKitGTK.
    expect(layer.declarations).not.toMatch(/z-index|mix-blend-mode|isolation|opacity|transform|filter/);
    const box = rules.find((r) => r.selector === ".visual-paint > div")!;
    expect(box.declarations).toMatch(/background:\s*var\(--nv-cursorline\)/);
    expect(box.declarations).toMatch(/mix-blend-mode:\s*lighten/);
    expect(rules.find((r) => r.selector === '.visual-paint[data-scheme="light"] > div')!.declarations)
      .toMatch(/mix-blend-mode:\s*darken/);
    expect(rules.find((r) => r.selector.includes(":not([data-visual=\"visual\"])") && r.selector.endsWith(".visual-paint"))!
      .declarations).toMatch(/display:\s*none/);
  });
});

/* A browser can break `--json` between its hyphens; neither hyphens nor word-break prevents
   it. No-wrap preserves short spans but overflows long ones. An atomic inline-block moves as
   one piece when it fits the row; longer spans are capped at 100% and may break anywhere.
   Fenced `pre code` stays outside the selector. jsdom has no layout: check the rule and that it
   reaches the renderer's span; a real-display check is needed for rendering. */
describe("inline code never breaks inside a token", () => {
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

/* At panel text 0.9, WebKitGTK's overlay scrollbar covers the last (+) line of a one-line
   expanded diff. It takes no space, so reserve padding below that line. `scrollbar-gutter`
   reserves block-axis scrollbar space only. jsdom draws none; check the room, not the pixels. */
describe("an expanded diff keeps room under its last line for its scrollbar", () => {
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

/* The reason box's intrinsic width is about twenty characters and clips its placeholder;
   WebKit's default 13.33px button font also ignores panel scaling. Fill the card's width with
   an ellipsis when needed, and give its buttons the panel face and token scale. jsdom has no
   layout, so these check rules; a real display must check the pixels. */
describe("the permission card's reason box and buttons follow the panel", () => {
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
    expect(declarations).toMatch(/font-size\s*:\s*var\(--fs-prose\)\s*;/);
  });

  it("Approve, Deny and the third button take the panel's face and --fs-prose, not WebKit's form-control font", () => {
    const declarations = declarationsFor(".permission-card-buttons button");
    expect(declarations).toMatch(/font\s*:\s*inherit\s*;/);
    expect(declarations).toMatch(/font-size\s*:\s*var\(--fs-prose\)\s*;/);
  });
});

/* Atomic inline code needs two exceptions. Table cells keep words whole, but a span's own
   overflow-wrap and max-width override inherited cell values, so reset both there. Atomic
   inline boxes also do not receive ancestor text decoration: set underline and strike-through
   explicitly. Inheriting would fail with an intervening strong element whose computed value
   is none. `winningDeclarationOn` checks the winners; jsdom draws nothing. */
describe("inline code keeps its cell's words and its links' decoration", () => {
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
    // Absolute against the stage (not the root), inset 8px from each of its edges.
    expect(winningDeclaration(html, ".trust-prompt", "position")).toBe("absolute");
    expect(winningDeclaration(html, ".trust-prompt", "inset")).toBe("8px");
  });
});

describe("the agent's text in the band and its detail", () => {
  /* Task descriptions are the agent's commands: drawn in storage order, so right-to-left letters in them cannot
     reorder their words (`cp א ב` read as `cp ב א`). */
  it("draws the background-task rows in storage order with the override fallback first", () => {
    const rule = rulesMatching(withoutComments, ".tasks-detail").find((r) => r.selector.includes(".tasks-row"))!;
    expect(rule.body).toMatch(/direction:\s*ltr\s*;/);
    const fallback = rule.body.search(/unicode-bidi:\s*bidi-override\s*;/);
    expect(fallback).toBeGreaterThanOrEqual(0);
    expect(rule.body.search(/unicode-bidi:\s*isolate-override\s*;/)).toBeGreaterThan(fallback);
  });
});

describe("thinking text", () => {
  /* A thought is the provider's text and may carry right-to-left runs or stray direction marks; its
     box is isolated so they cannot reorder the row's head or the rows around it, in both themes. */
  it("isolates the direction of a thought's text", () => {
    for (const cls of [".thinking-text", ".thinking-preview"]) {
      const bodies = rulesMatching(withoutComments, cls).map((rule) => rule.body);
      expect(bodies.some((body) => /unicode-bidi:\s*isolate\s*;/.test(body)), cls).toBe(true);
    }
  });
});

describe("the thinking preview's box", () => {
  /** Every declaration `.thinking-preview` gets (its own rule and the one it shares with
   *  `.thinking-text`), later ones winning. */
  function previewDeclarations(): Map<string, string> {
    const decls = new Map<string, string>();
    for (const rule of splitRules(stripComments(css))) {
      if (!rule.selector.split(",").some((s) => s.trim() === ".thinking-preview")) continue;
      for (const decl of rule.declarations.split(";")) {
        const m = decl.match(/^\s*([\w-]+)\s*:\s*(.+?)\s*$/s);
        if (m) decls.set(m[1], m[2]);
      }
    }
    return decls;
  }

  /** A length of the shapes this rule uses (`Nem`, `Npx`, `calc()` of sums and products of
   *  them) at a font size of `fontPx`. */
  function px(value: string, fontPx: number): number {
    const expr = value
      .replace(/^calc\((.*)\)$/s, "$1")
      .replace(/(\d+(?:\.\d+)?)em\b/g, (_, n) => `(${n}*${fontPx})`)
      .replace(/(\d+(?:\.\d+)?)px\b/g, "$1");
    if (!/^[\d\s.+*()-]+$/.test(expr)) throw new Error(`not a length this test reads: ${value}`);
    return Function(`return (${expr});`)() as number;
  }

  it("holds exactly the lines it shows, so the newest is never cut by the box's own padding", () => {
    const decls = previewDeclarations();
    const lineHeight = Number(decls.get("line-height"));
    const [padTop, , padBottom] = (decls.get("padding") ?? "").split(/\s+/).concat(["", ""]);
    const vertical = px(padTop, 1) + px(padBottom || padTop, 1);
    for (const font of [11, 14, 18]) {
      expect(px(decls.get("max-height") ?? "0", font)).toBeCloseTo(
        THINKING_PREVIEW_LINES * lineHeight * font + vertical,
        3,
      );
    }
  });

  it("keeps its bottom: lines that wrap past the box are cut at the top, the oldest end", () => {
    const decls = previewDeclarations();
    expect(decls.get("overflow")).toBe("hidden");
    expect(decls.get("display")).toBe("flex");
    expect(decls.get("flex-direction")).toBe("column");
    expect(decls.get("justify-content")).toBe("flex-end");
  });
});

it("isolates revealed question and subagent text and preserves their line breaks", () => {
  for (const selector of [".question-line", ".subagent-heading", ".subagent-status", ".subagent-warning", ".subagent-report"]) {
    const block = withoutComments.split("}").find((rule) => rule.split("{")[0].split(",").some((part) => part.trim() === selector));
    expect(block).toMatch(/unicode-bidi:\s*isolate\s*;/);
    expect(block).toMatch(/white-space:\s*pre-wrap\s*;/);
  }
});

/* Card mode's look: the accent ring on the card holding the keys, the digits on its answer buttons dim until card
   mode is armed and lit once it is, and the highlight drawn as a focused control's ring. It changes colours, opacity
   and outlines only, so it reads the same in every look that shares the panel's variables. */
describe("card mode's look", () => {
  const CARD = (cardClass: string, button = "") =>
    `<div class="agent-ui-root"><div class="message-list"><div class="row-current"><div class="${cardClass}">` +
    `<div class="permission-card-buttons">` +
    `<button type="button" class="${button}"><kbd class="card-digit armed">1</kbd> Approve</button>` +
    `<button type="button" class="${button}"><kbd class="card-digit">2</kbd> Deny</button>` +
    `<button type="button" disabled><kbd class="card-digit armed">3</kbd> Always allow</button>` +
    `</div></div></div></div></div>`;
  const cardModeRules = () =>
    splitRules(withoutComments).filter((r) => /permission-card-mode|card-digit|card-highlight/.test(r.selector));

  it("draws no data-key badge: out of card mode the buttons show no key", () => {
    expect(withoutComments).not.toMatch(/\[data-key\]/);
    expect(withoutComments).not.toMatch(/attr\(data-key\)/);
  });

  it("changes no size: any radius or height in its rules is a shared variable with a fallback", () => {
    const rules = cardModeRules();
    expect(rules.length).toBeGreaterThanOrEqual(3);
    for (const rule of rules) {
      for (const raw of rule.declarations.split(";")) {
        const m = raw.trim().match(/^([a-z-]+)\s*:\s*(.*)$/is);
        if (m === null) continue;
        expect(m[1], `${rule.selector}: ${m[1]}`).not.toMatch(/^(?:width|min-width|max-width|padding(?:-[a-z]+)?|margin(?:-[a-z]+)?|border-width)$/);
        if (/radius|height/.test(m[1])) expect(m[2].trim(), `${rule.selector}: ${m[1]}`).toMatch(/^var\(--nv-[a-z-]+,\s*[^)]+\)$/);
      }
    }
  });

  it("rings the card holding the keys in the accent, over the current card's warning frame", () => {
    const held = CARD("permission-card permission-card-mode");
    expect(winningDeclaration(held, ".permission-card", "box-shadow")).toBe("0 0 0 1px var(--nv-focus-ring)");
    expect(winningDeclaration(held, ".permission-card", "border-color")).toBe("var(--nv-focus-ring)");
    expect(winningDeclaration(CARD("permission-card"), ".permission-card", "box-shadow")).toBe("0 0 0 1px var(--nv-warn)");
  });

  it("draws the digits dim until armed and lit once armed; a disabled button's digit stays dim", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = CARD("permission-card permission-card-mode");
    const [armed, unarmed, disabled] = Array.from(document.body.querySelectorAll(".card-digit"));
    expect(winningDeclarationOn(armed, "opacity")).toBe("1");
    expect(winningDeclarationOn(unarmed, "opacity")).toBe("0.45");
    expect(winningDeclarationOn(disabled, "opacity")).toBe("0.45");
    expect(winningDeclarationOn(armed, "font-family")).toBe("var(--nv-font-mono)");
  });

  it("dims an unarmed digit less on Approve's solid face, so it still reads as a digit", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML =
      `<div class="permission-card permission-card-mode"><div class="permission-card-buttons">` +
      `<button type="button"><kbd class="card-digit">1</kbd> Approve</button>` +
      `<button type="button"><kbd class="card-digit">2</kbd> Deny</button>` +
      `</div></div>`;
    const [approve, deny] = Array.from(document.body.querySelectorAll(".card-digit"));
    expect(winningDeclarationOn(approve, "opacity")).toBe("0.6");
    expect(winningDeclarationOn(deny, "opacity")).toBe("0.45");
    approve.classList.add("armed");
    expect(winningDeclarationOn(approve, "opacity")).toBe("1");
  });

  it("rings the highlighted button as a focused control is ringed, Approve's solid face included", () => {
    document.head.innerHTML = `<style>${css}</style>`;
    document.body.innerHTML = CARD("permission-card permission-card-mode", "card-highlight");
    const [approve, deny] = Array.from(document.body.querySelectorAll<HTMLButtonElement>(".permission-card-buttons button"));
    for (const button of [approve, deny]) {
      expect(winningDeclarationOn(button, "outline")).toBe("2px solid var(--nv-focus-ring)");
      expect(winningDeclarationOn(button, "outline-offset")).toBe("3px");
    }
    // The highlight is a class, not focus: even a focused highlighted button keeps the ring over the global
    // `button:focus` rule that turns outlines off.
    deny.focus();
    expect(winningDeclarationOn(deny, "outline")).toBe("2px solid var(--nv-focus-ring)");
    // Without the class nothing is ringed.
    document.body.innerHTML = CARD("permission-card permission-card-mode");
    expect(winningDeclarationOn(document.body.querySelectorAll(".permission-card-buttons button")[1], "outline")).not.toBe(
      "2px solid var(--nv-focus-ring)",
    );
  });
});
