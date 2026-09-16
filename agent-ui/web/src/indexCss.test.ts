/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import css from "./index.css?raw";

const withoutComments = css.replace(/\/\*[\s\S]*?\*\//g, "");

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

  it("draws text only in --nv-fg or --nv-muted, the two colours Rust guards for text", () => {
    // --nv-warn/--nv-error/--nv-ok are guarded at 3:1 -- for borders, rules, tints and dots (WCAG
    // 1.4.11), not for text -- and --nv-mode-browse/--nv-mode-input are not guarded at all. Used as
    // 12px text on rose-pine dawn they measured 2.05:1 (a resume row's heading on its hover band) to
    // 3.84:1 (the session-lost banner), where the same text had been about 12:1 before the theme
    // pipeline. A signal colour goes on a border or a rule beside the text, never on the text.
    // (`(?<![a-z-])` so `background-color:`/`border-color:` are not read as text colours.)
    const declarations = withoutComments.match(/(?<![a-z-])color:[^;{}]*;/g) ?? [];
    expect(declarations.length).toBeGreaterThan(0);
    for (const declaration of declarations) {
      expect(declaration).toMatch(/^color: var\(--nv-(fg|muted)\);$/);
    }
  });

  it("does not dim a conversation-picker row with opacity", () => {
    // Every remembered-session row is unselected until clicked, since the picker opens on "New
    // session". Opacity multiplies text contrast down with it: fg at 0.7 is 3.34:1 on rose-pine
    // dawn and 2.93:1 on its hover band. The selected row is marked by its fill instead.
    const rules = withoutComments.match(/[^{}]*session-choice[^{}]*\{[^}]*\}/g) ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const rule of rules) {
      expect(rule).not.toMatch(/opacity/);
    }
  });
});
