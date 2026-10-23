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
import { Composer } from "./components/Composer";
import { Row } from "./components/Row";
import { THINKING_PREVIEW_LINES } from "./components/MessageList";
import type { ToolCallRecord } from "./types";

/** Strip CSS comments, but never a `/*` that is inside a string.
 *
 *  A bypass can sit UPSTREAM of every scan in this
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
/** A selector branch that sits inside the winbar, the status line or the status band -- `.winbar`,
 *  `.status-line`, `.status-band`, or one of them as an ancestor (`.winbar .identities`,
 *  `.status-band .band-open`, ...). The band joined 2026-10-16, when it took the editor's statusline
 *  fill (`--nv-chrome`) so the two read as one line across the divider. */
const CHROME_BRANCH = /(?:^|[\s.])(?:winbar|status-line|status-band)\b/;
/** The solid cursor block: the current row's sign cell, a focused button with its children (every
 *  control is keyboard-reachable and the selected one is drawn as the cursor), the band's own
 *  `↓N` button,
 *  the chooser's current row's sign cell, and CARET's own
 *  one-character block (the same reversed body pair, only on
 *  the selection it paints itself), and a permission card's primary button (2026-10-13: the chat page's
 *  card fills its first answer with the same reversed pair). Nothing else. */
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
/** The status band's mode block (2026-10-15), drawn in the editor's own statusline colours: a branch that is
 *  exactly `.band-mode`, optionally narrowed by `[data-mode="..."]`. Its text may be a `--nv-status-*-fg`
 *  only where the same rule paints that mode's `--nv-status-*-bg` -- the pair nvim's lualine draws, which is
 *  deliberately unguarded for the reason `--nv-syn-*` is: it is the editor's own colour. */
const BAND_MODE_BRANCH = /^\.band-mode(?:\[data-mode="[a-z]+"\])?$/;
/** Glyphs that carry a state in a signal colour (2026-10-13, the chat page): each selector exactly, with
 *  the one colour it may take. A glyph is not text, so the threshold that applies is the non-text one
 *  (3:1, WCAG 1.4.11): `--nv-ok`/`--nv-error` are guarded at 3:1 against `bg`, and `--nv-chrome-accent`
 *  -- the accent the activity line's bar meter was filled with before the star replaced it -- is guarded
 *  at 3:1 against both `chrome` and `bg` (`tokens.rs`; until 2026-10-13 against
 *  `chrome` alone, which on lunaperche left it black on a black `bg`). The numbers beside the edit signs
 *  stay in the head's ink, and a diff line's text stays in its own ink beside the colour of its `+`/`-`. */
const GLYPH_COLOURS: Record<string, string> = {
  ".activity-glyph": "--nv-chrome-accent",
  '.row-tool[data-sign="◐"]::before': "--nv-chrome-accent",
  '.row-tool[data-sign="✗"]::before': "--nv-error",
  ".edit-count-added .edit-sign": "--nv-ok",
  ".edit-count-removed .edit-sign": "--nv-error",
  ".diff-added .diff-gutter": "--nv-ok",
  ".diff-removed .diff-gutter": "--nv-error",
  ".permission-card .diff-added .diff-gutter": "--nv-ok",
  ".permission-card .diff-removed .diff-gutter": "--nv-error",
};
/** A selector branch inside one of the boxes painted `--nv-surface` (sw-theme-2): the which-key box,
 *  a permission card's edit head, the terminal handoff's command, a fenced code block, a tool's output box
 *  and a table's head (since 2026-10-14 a tool line, an output, a table's head and a permission card are not
 *  surface-painted, but their names stay matched). Where the
 *  text under such a selector actually lands is proven by rendering, in "text on a --nv-surface
 *  fill" at the bottom of this file; this only keeps the surface pair from being declared anywhere
 *  else. */
const SURFACE_BRANCH =
  /(?:^|[\s.])(?:which-key-box|tool-card(?:-[a-z]+)?|permission-card(?:-[a-z-]+)?|handoff-command|code-block|tool-result-(?:preview|body)|table-scroll th)\b/;
/** The three tinted fills (2026-10-13), each with the one pair `tokens.rs` guards against
 *  it, and the selector branches that sit on it: your own tint (`--nv-tint`: your message, the composer
 *  and what is in it, an inline code span in a reply), a permission card's warn tint, and a failure's
 *  error tint (a failed call's output, the lost-session card in the foot). As with `SURFACE_BRANCH`, where
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
    branch: /(?:^|[\s.])tool-result-error\b|^\.agent-ui-conversation > \.row-error\b/,
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
      // Text on a card: `--nv-surface-fg`/`--nv-surface-muted`, guarded against `surface` in Rust
      // (sw-theme-2), and only inside the boxes that paint it.
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

/** Every top-level `--custom-property: value;` declared inside a `:root { ... }` block, name (with
 *  its leading `--`) to raw declared-value TEXT. This is the one piece of `var()` this suite is
 *  allowed to resolve, and it is resolved from the stylesheet's own source rather than from
 *  anything jsdom computed: jsdom's `getComputedStyle` never substitutes `var()` at all (see the
 *  panel-width block below for the measurement that proves it), so `expandVars` below has to look
 *  the tokens up itself. A flat regex is enough because these declarations are not nested (the
 *  "contains no nested blocks today" test above is what keeps that assumption checked).
 *
 *  The `s` (dotAll) flag is needed on the per-declaration regex because a
 *  declaration's VALUE may span multiple lines:
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
 *  matching the fraction as text. The text scale is one number and five ratios of
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

/** Custom properties declared on exactly `.row` -- none since 2026-10-13, when `--row-inline-size` (the
 *  row's width, derived from the list's measured one) went with the escapes that spent it. Read from the
 *  stylesheet's own source, like `ROOT_TOKENS`, so a test can say there are none. */
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
 *  The `i` flag and the widened lookbehind handle uppercase declarations:
 *  CSS property names are ASCII case-insensitive (CSS Syntax L3), so `TRANSITION:`
 *  is live CSS that a case-sensitive matcher reads as nothing at all. Nobody writes it by
 *  hand -- it is closed because this guard's whole claim is a property of the FILE, and a
 *  property with a spelling that escapes it is not established. An ESCAPED ident is still
 *  NOT matched: decoding those needs a real tokenizer, and that limit is recorded in the
 *  control below so a passing scan does not overstate its coverage. */
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
 *  literal pill can escape both: `background` + `border-radius: 12px`
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

/** A shorthand's own longhands, for the shorthands `winningDeclaration` below is actually asked to
 *  check. A rule that sets only `border-left-style`/`border-left-width` never sets the literal
 *  string `"border-left"` at all, so asking the CSSOM for that one property name is blind to it --
 *  an equal-specificity, later `border-left-style: none; border-left-
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
 *  bypass: `.row-body { border-left: none !important; }`, lower specificity than
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
    // `--nv-chrome-fg`/`--nv-chrome-muted` inside the
    // winbar/status-line (guarded against `chrome` instead -- a reversed StatusLine makes `chrome`
    // Normal's `fg`, so checking against `bg` would measure the wrong pair). Plus one exemption
    // that is NOT a contrast guarantee at all: `--nv-syn-*` inside a
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

  it("admits each tint's pair as text only on that tint's own branches, every branch of them", () => {
    expect(unguardedTextColorDeclarations(".row-prompt .row-body { color: var(--nv-tint-fg); }")).toEqual([]);
    // The composer left the tint on 2026-10-15: the tint's pair is refused there now.
    expect(unguardedTextColorDeclarations(".composer-browse-hint { color: var(--nv-tint-muted); }")).toEqual(["color: var(--nv-tint-muted);"]);
    expect(unguardedTextColorDeclarations(".permission-card-description { color: var(--nv-warn-tint-muted); }")).toEqual([]);
    expect(unguardedTextColorDeclarations(".tool-result-error .tool-result-truncated { color: var(--nv-error-tint-muted); }")).toEqual([]);
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
    // is gone; these two replace it and the old status line. `.status-row`
    // and `.panel-footer` are themselves gone now,
    // folded into `.status-band`.
    const rules = withoutComments.match(/\.(?:status-band|activity-line)[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });

  it("the status band is one editor row high, and never grows", () => {
    // The status band must align with the editor's status line. A min-height lets children,
    // especially the fallback font for the mode glyph, make the band taller. --nv-editor-row
    // supplies the editor's cell height; fixed height and overflow: hidden keep the band at
    // that height.
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
    // Since 2026-10-13 the band has no top rule -- the composer card above it is the foot's boundary --
    // so nothing is taken out of the fixed height and the line box is the whole row, on the same centre
    // line as the editor's statusline text.
    expect(decl("border-top")).toBeUndefined();
    expect(decl("line-height")).toBe("var(--nv-editor-row, 24px)");
  });

  /* In-flight motion needs the structural property "the
     element is absent" <=> "no animation is running" has to be a property of the FILE, not of this
     document, so a second animated selector or a second @keyframes reusing the idea would be a
     silent second motion nobody argued for (there is no second animation for a second
     purpose).

     **The guard needs negative controls for bypasses in splitting and declaration matching
     against the real file (three) and three more
     against the rewrite.** All six are kept below as negative controls, fed to the same splitter
     the real assertions use -- as STRINGS rather than as real CSS, so a future author who reopens
     one of them fails a test here instead of shipping an unargued motion:

       (a) `animation-name` / `animation-duration` longhands on a second selector. The detector was
           `/(?<![a-z-])animation:/`, the SHORTHAND only, so the longhand spelling of the same thing
           was invisible.
       (b) `transition: background-color 300ms` on any selector. The word `transition` appeared
           nowhere in this file, and animating a COLOUR is exactly what the motion rule forbids (an
           animated fill spends most of its period below the accent's 3:1 contrast) -- and the
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

  it("puts every animation OR transition declaration on .activity-glyph[data-moving], with exactly one @keyframes", () => {
    // 2026-10-13: the activity line's star replaced the bar meter as the one moving thing (the chat
    // page's look); the guard moved with it unchanged.
    const moving = movingSelectorsIn(css);
    expect(moving.length).toBeGreaterThan(0);
    for (const selector of moving) {
      expect(selector).toBe(".activity-glyph[data-moving]");
    }
    const keyframeBlocks = withoutComments.match(/@keyframes\s+[\w-]+\s*\{/g) ?? [];
    expect(keyframeBlocks).toHaveLength(1);
    expect(keyframeBlocks[0]).toContain("turn-breath");

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
    // the contrast floor `tokens.rs` guarantees only at full strength (before any opacity is applied).
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
    // Negative control (g): property names are
    // ASCII case-insensitive per CSS Syntax L3, so this is live CSS that the case-sensitive
    // matcher read as nothing at all. Injected into the real stylesheet it left the suite green.
    expect(movingSelectorsIn(".tool-card { TRANSITION: background-color 300ms linear; }")).toEqual([
      ".tool-card",
    ]);
    // And the limit: an ESCAPED ident is live
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

  it("rests the star at its full size under reduced motion, and breathes in exactly four steps otherwise", () => {
    // The star replaced the bar meter on 2026-10-13. Its cycle is two halves, each `steps(2, jump-end)`:
    // progress {0, 1/2} per half, so the scale runs full, halfway down, the bottom, halfway back up --
    // four visible states, one per `--meter-step`, never eased (an eased fade or pulse repaints every
    // frame, which the typing cadence exists to prevent).
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

    // Negative control (as first written): an opacity fade, both declarations `;`-terminated.
    const fade = "\n  from { transform: scaleX(0.25); opacity: 0.4; }\n  to { transform: scaleX(1); opacity: 1; }\n";
    expect(keyframeDeclarations(fade).some((d) => !d.startsWith("transform:"))).toBe(true);

    // Negative control (c): a final
    // declaration with no trailing `;`, which is how a last declaration is most often written.
    // The second `expect` is the point of this control -- it runs the OLD pattern over the same
    // string and shows it finds nothing wrong, so this is a hole that was really open rather than
    // one this test merely asserts about.
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

/** Checks which declaration wins by attaching the real stylesheet to a DOM and reading
 * getComputedStyle. Matching rule text alone misses specificity conflicts: .row-assistant pre
 * can override .code-block, and mode-selector button rules can override choice-row layout and
 * selection fills. Negative controls restore the competing rules to verify that the assertions
 * detect them. jsdom does not resolve var(), so these assertions compare token names rather
 * than rendered colours. It also drops shorthands containing var(), including border and
 * border-left; longhands such as border-left-color remain observable. When a higher-specificity
 * var() competes with a lower-specificity literal, this engine uses source order. The
 * boxed-button background check would therefore pass even with the conflicting rule restored.
 * Its test uses border-radius instead, which contains no var() and changes when the conflict
 * returns. The code-block and selected-row background checks have var() on both sides. jsdom
 * applies specificity correctly in that case regardless of source order. Browser rendering
 * still needs to be checked separately. */
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
  // `.panel-footer .mode-block` is `.band-mode` now, a bare selector
  // (no wrapper prefix -- `StatusBand.tsx` never nests it under a `.panel-footer`, which is gone).
  const MODE_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input">INPUT</span></div>`;
  const UNFOCUSED_INPUT_BLOCK = `<div class="status-band"><span class="band-mode" data-mode="input" data-focused="false">INPUT</span></div>`;

  // The conversation used to be `grid-template-rows: auto 1fr auto auto`, which
  // gives the `1fr` to the SECOND child -- the fatal-error banner when it is shown, not the list.
  // The list then took its full content height and never scrolled, so `j`/`k` could not step
  // through a long reply over a dead session. jsdom has no layout, so this pins the rule that
  // decides it: the list grows by its own class, whatever sits between it and the rest of the
  // panel: the activity line, the composer and the band.
  // The band replaces the separate status row and footer,
  // so it is also separate from the scrollable list.
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

  /** The unread pill is the band's own `.band-unread`, drawn on the
   *  band's right with `↓N` inverted, never over text. It is not floated absolutely over the
   *  list -- the opposite invariant from the one this test used to pin. */
  it("keeps the jump-to-bottom pill in normal flow inside the band, not floated over the list", () => {
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

  /* --- no width limit ---------------------------------------------------------------------------
     Until 2026-10-13 fenced code, tool output and a permission card's input and diff each "escaped" the
     prose measure to the row's right edge, through a width the page measured (`--list-inline-size`,
     before 2026-09-24 a `100cqw` against a query container on `.row`, which made WebKitGTK reset the
     list's scroll position while a reply streamed). The chat page then sat in a centred column
     (`--column-w`) that a wide panel could only be exceeded by code and tables. Since 2026-10-14 there is
     no column: the conversation and the foot fill the panel between two side margins
     (`--page-pad`, a viewport unit clamp), so nothing measured is needed, nothing reaches past anything,
     and no container can come back for it. */
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
    // The composer is centred: the same margin each side (2026-10-15).
    expect(declarationsOf(".composer")).toContain(`margin: 10px ${SIDE} 14px;`);
    // None of them is centred by a width and an auto margin instead: that would centre the list's box, not
    // its rows, and leave the foot and the conversation on different edges.
    const FILLS = [".message-list > .row", ".turn-time", ".activity-line", ".queue-lines", ".turn-changes", ".composer", ".agent-ui-conversation > .row", ".history-notice"];
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

  /* The in-flight indicator must stay visible on a narrow line
     Two defects, one bar. jsdom runs no flex layout, so both tests read the DECLARATIONS that
     decide the outcome -- which is the same thing every other test in this block does, and is
     exactly why the defects were invisible until someone did the flexbox arithmetic by hand. */

  it("keeps the star from being squeezed off a narrow activity line", () => {
    // The star is the line's one moving mark (2026-10-13; the bar meter before it). A flex item shrinks
    // by default, and the narrow panel is where the reassurance matters most, so it is `flex: none` --
    // the same idiom `.diff-gutter` uses.
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

  /** The scale's arithmetic test reads `ROOT_TOKENS`, which is built by the
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
    // No exception any more: `--row-inline-size`, declared on `.row` from 2026-09-24 to 2026-10-13 (the
    // row's measured width), went with the escapes that spent it. Every panel token is a `:root` knob.
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
    // The six values this file used to hardcode, and the ratio each token's own comment says it
    // replaced -- evaluated, not string-matched (see `evaluateFsExpression`'s own doc for why a text
    // match on `"* 12 / 14"` cannot tell a correct ratio from a wrong one).
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

    // The heading step (2026-10-13) is one twentieth over the reply's prose, by the same arithmetic.
    expect(evaluateFsExpression(ROOT_TOKENS.get("--fs-heading")!)).toBeCloseTo(12 * 1.05, 10);
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
    // `var(--nv-font-size, 14px)` adds a fallback to the bare-variable form; every
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
    // thing that can give. The star, the clock and Stop are never spent. The Stop button is the one
    // that matters -- App.tsx's own comment says it is the ONLY Stop control for mouse users, so it
    // going off the right edge is the interrupt affordance leaving the screen.
    //
    // The indicator is the only part of this row that can yield space. The
    // mode block, the session status word and the position counter left `.status-line` entirely (the
    // status word is no longer shown; the position moved into `StatusRow`'s own text) -- so
    // there is no longer a second thing in the row that could compete with the indicator for space,
    // and only the indicator's own shrink policy determines whether it stays visible.
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
    // Negative control: the state this replaced -- nothing shrinking, nothing clipping -- is what
    // pushed Stop off the edge, and it is one later `flex` declaration away.
    expect(computed(html, ".turn-state", ".activity-line .turn-activity .turn-state { flex: none; }").flexShrink).toBe("0");
  });


  it("keeps the sign column on the same track whether a grid row's body is prose or a wide code block", () => {
    // On the sign-column grid -- the rows outside the conversation, such as the turn review's, since
    // 2026-10-13 -- the entire reason the grid exists: the sign column must sit
    // at the same x position down the whole page. Widening the BODY column must never be able to
    // move it, so this compares `.row`'s own `grid-template-columns` -- the declaration that
    // decides column 1's width -- across a plain-prose row and a row whose body is a wide code
    // block. jsdom does not resolve `var()`, so both sides come back as the literal token text
    // (`"var(--row-sign-w) minmax(0, var(--prose-measure))"`) regardless of which fixture produced
    // them -- `expandVars` (top of file) resolves that against the real `:root` declarations so the
    // pin is still on the real, current value (`--prose-measure: 1fr`) and not on the unresolved
    // string, which would pass even if `.row`'s own selector stopped applying at all.
    const proseRow =
      `<div class="review-overlay"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><p>prose</p></div></div></div>`;
    const codeRow =
      `<div class="review-overlay"><div class="row row-assistant">` +
      `<span class="row-sign">›</span><div class="row-body"><pre class="code-block"><code>x</code></pre></div></div></div>`;
    const proseColumns = expandVars(computed(proseRow, ".row").gridTemplateColumns);
    expect(proseColumns).toBe("20px minmax(0, 1fr)");
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

  it("keeps the grid for rows outside the conversation and gives the conversation's rows a sign cell and a body of their own, with no size container", () => {
    // Until 2026-10-13 this test pinned `--row-inline-size`, the row's width derived from the list's
    // measured one, which every wide-content escape spent; before 2026-09-24 it pinned the opposite of
    // what it pins now, `container-type: inline-size`, the container that made WebKitGTK drop the reader
    // while a reply streamed. The chat page's rows spend nothing measured. Between 2026-10-13 and
    // 2026-10-14 they were blocks in a centred column with no sign cell; they are now a grid of a
    // generated sign cell (`--sign-w`) and a body, on one left edge.
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

  /* --- the sign-column geometry, as tokens, is actually derived (2026-09-21) -------------------
     `--row-gutter` replaced independent magic numbers (`.row`'s `22px`/`8px` and the `30px` other rules
     subtracted) that used to have to agree by hand. Since 2026-10-13 its consumers are the review
     overlay's indented lines, which line up with that grid; the conversation's rows left the grid. These
     guard that the formula really is a formula, that its consumers spend it, and that no custom property
     in the file carries a container-query unit (the WebKitGTK reason). */

  it("derives --row-gutter from the sign column instead of copying its sum, and every indent that lines up with the grid spends it", () => {
    // Half 1: the declared value is a formula over the two knobs that make up the sign column, not a
    // copy of their sum -- `calc(28px)` would satisfy every other test in this file just as well.
    const gutter = ROOT_TOKENS.get("--row-gutter");
    expect(gutter).toBe("calc(var(--row-sign-w) + var(--row-gap))");
    expect(gutter).toMatch(/var\(--row-sign-w\)/);
    expect(gutter).toMatch(/var\(--row-gap\)/);
    // Half 2: a correct formula sitting unused proves nothing. Its consumers are the review overlay's lines
    // that sit under a grid row's body; each spends the token, and nothing in the file spells the sign
    // column's width (20px plus the 8px gap, or the older 30px / 32px sums) as a literal in a calc.
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
    // Since 2026-09-24 there is no container-query unit anywhere in the file (the tripwire near the
    // top of the `index.css` block: with no container, a `cq*` unit would silently resolve against
    // the viewport). This guard is kept for what it pins about custom properties specifically, and
    // because its bypasses are still real ones. What follows is its original rationale.
    //
    // Spelled out in index.css's own comment: a `var()` substitutes as raw tokens, so a `cqw` unit
    // written INSIDE a custom property's value still substitutes as raw tokens, but
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
    // Negative control, an override outside the top-level root (adapted to a real token name --
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

    // A second bypass of the SAME guard is a multi-line VALUE rather than an unusual
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
    // asserted from the comment alone, using the same expression as the working parser
    // with only the flag removed.
    const preFixRegex = /^\s*(--[\w-]+)\s*:\s*(.+?)\s*$/;
    expect("\n  --bypass: calc(\n    100cqw\n  )\n".match(preFixRegex)).toBeNull();
  });


  /* --- your own message (2026-10-13) ---------------------------------------------------------------
     From 2026-09-21 to 2026-10-13 your message was an indent with a rule down its left edge, and these
     tests kept it OFF the bubble vocabulary (no fill, no radius, no `fit-content`), to take Cursor's idea
     without its look. The chat page's look (2026-10-13) makes it the shape every mainstream agent UI
     uses: a tinted block on the right, on the same surface as the composer. These tests now pin that shape, the cascade that keeps it, and the
     two properties the indent's tests carried that still matter -- the text colour is guarded against the
     fill under it (since 2026-10-13 the tint's own `--nv-tint-fg`), and a long unbroken token breaks inside the block instead of scrolling the conversation. */
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
  /* 2026-10-15: the things a reader reviews or answers are framed and nothing else is -- an edit, a
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
    // `.band-mode[data-mode="input"]`, not `.panel-footer .mode-block
    // [data-mode="input"]` -- one class plus one attribute now that the wrapper prefix is gone.
    expect(specificity('.band-mode[data-mode="input"]')).toEqual([0, 2, 0]);
    expect(compareSpecificity([0, 3, 0], [0, 2, 0])).toBeGreaterThan(0);
  });

  // "keeps a choice row off the boxed-button look" (the `.mode-selector button:not(.row-choice)`
  // vs. `.row-choice` specificity regression) no longer applies to the current screen:
  // the mode-selector start screen's boxed Auto/Bypass buttons are gone along with the screen
  // itself (the empty tab never draws a boxed button), so `index.css` no longer carries that
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

  /* The band's mode block and the editor's statusline mode
     section beside it look the same; only the word differs. */
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
    // Without the keys (2026-10-16): no fill and no arrow, whatever the mode -- an inactive statusline has no
    // mode section, and an empty filled block read as a rendering fault. Its place stays, so nothing shifts.
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

  /* 2026-10-16: the start screen's box sits where the conversation's box will be, so a first send moves nothing. */
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

  /* 2026-10-16: the start screen's box keeps the bright frame only while the panel holds the keys. */
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

  /* 2026-10-16: with no config the editor's statusline sat one row above the panel's band (nvim's command line
     takes the bottom row) and was grey where the band had no fill, so the line broke at the divider. */
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
    // Chrome-guarded text is admitted on the band now that it sits on chrome.
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
    // Until 2026-10-13 the row's own sign cell was the solid cursor and stepped back to hollow while a
    // control inside the row had focus. The conversation's cursor is the band now, a soft fill rather
    // than a solid block, so the focused button is the only solid mark and the band stays.
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

  /* A HINT landing on a code block is "the item" until the next key (`y` copies it,
     `v` starts inside it), and it is now drawn: the block outlined in the body's ink, thinner while the
     panel does not have the keys. (Until 2026-10-13 the row's solid sign cell also went hollow meanwhile,
     through a `data-code-landed` mark on the list; the chat page's cursor is a soft band under the row,
     never a second solid mark, and the mark is gone.) */
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

  /** The chooser's current row carries the panel's
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
   *  the line, not a boxed button that WebKit would otherwise give its own
   *  button face. Focused, it is still the solid cursor every focused control is. */
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
     rounded a little more than the conversation's blocks, the prompt `›` in a cell before the text. */
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
  /** The composer is one framed box on the page; nothing inside it draws a second box. Previously,
   *  BROWSE's stand-in was boxed, and WebKit's own focus ring drew a box round
   *  the textarea in INPUT. */
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

  /* WebKitGTK drew "Ask the agent...— i or Ctrl+j to type" -- the
     hint span is a flex item of `.composer-browse-hint`, and a flex item's leading white space is
     removed, so the space `Composer.tsx` puts before the dash never showed. jsdom has no layout; this
     pins the rule that keeps it. */
  /* 2026-10-15: BROWSE's stand-in ends in the key alone, at the box's right end; with messages queued their
     count comes first and the key right after it. */
  it("puts BROWSE's key at the box's right end, after a queued count when there is one", () => {
    const html = (queued: boolean) =>
      `<div class="composer"><div class="composer-browse-hint">Ask the agent...${queued ? '<span class="composer-queued">+2 queued</span>' : ""}<span class="composer-key">i</span></div></div>`;
    expect(winningDeclaration(html(false), ".composer-key", "margin-left")).toBe("auto");
    expect(winningDeclaration(html(false), ".composer-key", "color")).toBe("var(--nv-muted)");
    expect(winningDeclaration(html(true), ".composer-key", "margin-left")).toMatch(/^0(px)?$/);
    expect(winningDeclaration(html(true), ".composer-queued", "margin-left")).toBe("auto");
  });

  /* The missing-sidecar remedy ("Looked for it at:\n  - <path>\n
     Install ...") ran together into one line. */
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

  /** At ~348px beside a Lua panel the empty tab scrolled
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
/* Long Bash commands, unbroken paths, URLs and patterns can push the conversation sideways when
   their <pre> keeps white-space: pre. Prose URLs and markdown tables need overflow handling
   too. jsdom has no layout, so these tests check that list text can break and every rendered
   <pre> wraps or scrolls within its own box. shell/tests/panel_stream_scroll.rs measures the
   result in WebKitGTK. */
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
      // A Bash card now renders `.permission-card-command`, not `.permission-card-input`
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
    // In the failure's own full-strength ink -- the error tint's guarded text, the block it sits on --
    // which the line inherits from the preview.
    expect(winningDeclarationOn(line.parentElement!, "color")).toBe("var(--nv-error-tint-fg)");
    expect(winningDeclarationOn(line, "color")).toBeFalsy();
  });

  it("holds the preview's lines in one zero-minimum track beside the connector, which is what keeps a long line from widening the row", () => {
    const preview = foldedPreview();
    expect(winningDeclarationOn(preview, "display")).toBe("grid");
    expect(winningDeclarationOn(preview, "grid-template-columns")).toBe("var(--sign-w) minmax(0, 1fr)");
    // The lines sit in the second track; the `⎿` is the first, generated content.
    expect(winningDeclarationOn(preview.querySelector(".tool-result-preview-line")!, "grid-column")).toBe("2");
  });

  it("draws a call's output under a muted connector with no fill, no frame and no rail, and a failure's in a framed block on the error tint", () => {
    const ok = foldedPreview();
    // On `--nv-bg`, so the body's muted ink, and the count is muted too.
    expect(winningDeclarationOn(ok, "color")).toBe("var(--nv-muted)");
    expect(winningDeclarationOn(ok.querySelector(".tool-result-preview-more")!, "color")).toBe("var(--nv-muted)");
    const failed = foldedPreview("", true);
    expect(failed.classList.contains("tool-result-error")).toBe(true);
    // A failure's output is the one framed output: on the error tint, outlined all round (never a rail down
    // one side), its text the tint's guarded pair, the frame in place of the connector, and still one
    // zero-minimum track so a long line cannot widen the row.
    expect(winningDeclarationOn(failed, "border-left")).toBeFalsy();
    expect(winningDeclarationOn(failed, "border")).toBe("1px solid color-mix(in srgb, var(--nv-error) 50%, var(--nv-bg))");
    expect(winningDeclarationOn(failed, "background")).toBe("var(--nv-error-tint)");
    expect(winningDeclarationOn(failed, "color")).toBe("var(--nv-error-tint-fg)");
    expect(winningDeclarationOn(failed, "grid-template-columns")).toBe("minmax(0, 1fr)");
    expect(winningDeclarationOn(failed, "width")).toBe("max-content");
    expect(winningDeclarationOn(failed, "min-width")).toBe("min(100%, 36rem)");
    expect(winningDeclarationOn(failed, "max-width")).toBe("100%");
    expect(winningDeclarationOn(failed.querySelector(".tool-result-preview-line")!, "grid-column")).toBe("1");
    // The count stays muted on a failure, in the tint's muted ink.
    expect(winningDeclarationOn(failed.querySelector(".tool-result-preview-more")!, "color")).toBe("var(--nv-error-tint-muted)");
    // The connector is generated content in the first track, muted, and never read out.
    const connector = splitRules(withoutComments).find((r) => r.selector === ".tool-result::before,\n.tool-result-preview::before" || r.selector.replace(/\s+/g, " ") === ".tool-result::before, .tool-result-preview::before");
    expect(connector?.declarations).toMatch(/content:\s*"⎿" \/ "";/);
    expect(connector?.declarations).toMatch(/color:\s*var\(--nv-muted\);/);
    // Nothing in the conversation or the foot draws a coloured rail down a side any more: what is left of a
    // `border-left` in a signal or hairline colour belongs to the start screen's cards and the trust prompt.
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

/** `tokens.rs` derives `--nv-surface-fg` (and `--nv-surface-muted`)
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

/** 2026-10-13: the chat page's three tints (your own `--nv-tint`, a permission card's
 *  `--nv-warn-tint`, a failure's `--nv-error-tint`) are mixed in Rust, and `tokens.rs` guards one text pair
 *  against each. Before that, muted text measured 3.6-4.8:1 on them (the body's `--nv-muted` is guarded
 *  against `bg` only), the composer's dimmed hint 2.3-3.0:1 and its placeholder WebKit's `darkGray`, about
 *  1.9:1 on a light tint. This renders the panel's real tinted boxes, finds each text's nearest fill, and
 *  requires every text on a tint to be drawn in that tint's pair -- and the pairs nowhere else. */
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
    // Your messages and an inline code span; the composer left the tint on 2026-10-15.
    expect(seen["var(--nv-tint)"] ?? 0, "your own tint").toBeGreaterThan(2);
    expect(seen["var(--nv-warn-tint)"] ?? 0, "the warn tint").toBeGreaterThan(8);
    // A failed call's output has no fill since 2026-10-14; the lost-session card is what is left on this tint.
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
    // The composer is on `--nv-bg` since 2026-10-15, so the body's pair, which Rust guards against it.
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
    expect(found).toEqual(new Set(VISUAL_CHROME));
  });

  it("highlights a selection in nvim's Visual colour while VISUAL/V-LINE is on, and turns off the cursorline fill where the two coincide", () => {
    // Comments stripped first: `rulesMatching` is a naive substring matcher, and this file's own
    // doc comment on the rule below mentions ".row-current" in backticks -- without stripping,
    // that comment text (attached to the WRONG rule's captured selector) is what "found" it.
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
      ".message-list[data-visual] ::selection",
    ]);
    for (const rule of rules) {
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

/* An atomic inline-block keeps an inline code token such as --json together and moves it to the
   next line when needed. Unlike white-space: nowrap alone, max-width: 100% with overflow-wrap
   lets a span longer than the row break within the row. The selector leaves fenced pre code
   alone. jsdom has no layout, so these tests check the declarations and that they reach the
   markdown renderer's inline spans. Rendering needs to be checked in a browser. */
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

/* At panel text 0.9 an expanded one-line
   edit diff's horizontal scrollbar was drawn over its last (`+`) line. The scrollbar is WebKitGTK's overlay one, so
   it takes no room of its own: the diff box (`.permission-card-diff`, the `pre` that scrolls sideways) keeps a strip
   of padding under its last line for it to sit in. `scrollbar-gutter` does not help, it reserves the block-axis
   scrollbar's room only. jsdom draws no scrollbar, so this pins the room, not the pixels. */
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

/* The permission card's reason box
   kept the browser's intrinsic width -- about twenty characters -- so its placeholder read "Reason (shown to the ag"
   -- and Approve/Deny kept WebKit's own 13.33px form-control font whatever the panel's text size was, so at a larger
   panel size the prose, the card's title and the reason box grew and the two buttons did not. The box now fills the
   card (an ellipsis rather than a hard clip where even that is too narrow), and the buttons take the panel's face and
   its `--fs-*` scale like every other control in it (the composer, the rename box). jsdom has no layout: this pins the
   rules, and the pixels are owed a screen. */
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

/* The atomic layout above reaches every inline span under an
   assistant row, which broke two things it was never meant to touch. (a) A table cell's words stay whole
   (`.table-scroll td { overflow-wrap: normal }`, T1), but the span's own `overflow-wrap: anywhere` and
   `max-width: 100%` beat the cell's inherited value, so an identifier in a cell broke again and was capped to
   the cell's width. (b) An inline-block does not take its ancestors' text decoration into its own box (CSS
   Text Decoration 3: "not propagated to ... atomic inline-level descendants"), so the code in a link lost its
   underline and the code in `~~ ~~` its strike-through -- each takes it back on its own box, explicitly
   rather than by `inherit`, since a `<strong>` between the link and the span would have a computed `none`.
   `winningDeclarationOn` reads which rule wins; jsdom draws nothing. */
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
    // Absolute against the stage (not the root), covering all of it.
    expect(winningDeclaration(html, ".trust-prompt", "position")).toBe("absolute");
    expect(winningDeclaration(html, ".trust-prompt", "inset")).toBe("8px");
  });
});

describe("the band's Ctrl+y segment", () => {
  /* It names what the agent asked to run, in the agent's own text: right-to-left letters in it must not
     reorder its words (`cp א ב` read as `cp ב א`) or the segments beside it. */
  it("is drawn in storage order, left to right, and isolated", () => {
    const rules = rulesMatching(withoutComments, ".band-approve").filter((rule) => rule.selector.trim() === ".band-approve");
    expect(rules.length).toBe(1);
    const body = rules[0].body;
    expect(body).toMatch(/direction:\s*ltr\s*;/);
    // The fallback first, the isolating override last, so an engine that knows it uses it.
    const fallback = body.search(/unicode-bidi:\s*bidi-override\s*;/);
    const isolating = body.search(/unicode-bidi:\s*isolate-override\s*;/);
    expect(fallback).toBeGreaterThanOrEqual(0);
    expect(isolating).toBeGreaterThan(fallback);
    expect(body).not.toMatch(/unicode-bidi:\s*isolate\s*;/);
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
