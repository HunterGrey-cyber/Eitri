// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, describe, expect, it } from "vitest";
import css from "./index.css?raw";
// The Rust side of the theme contract, read as TEXT at test time. See
// "every --nv-* this stylesheet reads is one Rust actually emits" at the bottom of this file for
// why the allowed names are derived from this source rather than written down here.
import tokensRs from "../../../core/src/theme/tokens.rs?raw";

const withoutComments = css.replace(/\/\*[\s\S]*?\*\//g, "");

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
    for (const declaration of body.match(/(?<![a-z-])color:[^;]*;/g) ?? []) {
      if (onHljsSurface && /^color: var\(--nv-syn-[a-z]+\);$/.test(declaration)) continue;
      if (onChromeSurface && /^color: var\(--nv-chrome-(fg|muted)\);$/.test(declaration)) continue;
      // The panel's cursor: `--nv-bg` text on an `--nv-fg` fill, the body's own pair reversed.
      // Only where the SAME rule paints that fill, so the exemption cannot outlive the fill.
      if (onCursorSurface && /^color: var\(--nv-bg\);$/.test(declaration)) continue;
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
    `<div class="agent-ui-root agent-ui-conversation"><div class="winbar">w</div>${banner}<div class="message-list">m</div><div class="status-line">s</div><div class="composer">c</div></div>`;

  it("gives the free height to the message list, fatal banner or not", () => {
    for (const banner of ["", `<div class="fatal-error">e</div>`]) {
      const list = computed(CONVERSATION(banner), ".message-list");
      expect(list.display).not.toBe("grid");
      expect(list.flexGrow).toBe("1");
      expect(list.minHeight).toBe("0px");
      expect(list.overflowY).toBe("auto");
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

  it("paints a code block on --nv-surface even inside an assistant row", () => {
    expect(computed(CODE_BLOCK, "pre").background).toBe("var(--nv-surface)");
    // The negative control: the rule that was actually deleted, put back. Without it this
    // assertion could be passing because nothing competes, which is not the same as winning.
    expect(computed(CODE_BLOCK, "pre", ".row-assistant pre { background: var(--nv-bg); }").background).toBe(
      "var(--nv-bg)",
    );
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
