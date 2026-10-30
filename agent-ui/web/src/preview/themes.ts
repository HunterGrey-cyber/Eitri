// Theme packets for the preview page, shaped like the ones `core/src/theme/tokens.rs` derives from
// a running nvim. The colours are each scheme's own highlight values, picked by hand.

type Palette = {
  bg: string; fg: string; surface: string; surfaceFg: string; muted: string; border: string; cursorline: string;
  chrome: string; chromeFg: string; accent: string; warn: string; error: string; ok: string;
  browse: string; input: string; hintBg: string; hintFg: string; search: string; light: boolean;
  syn: Record<string, string>;
  /** lualine's mode sections (`lualine_a_normal`/`_insert`/`_visual`) for the scheme, `[bg, fg]`, which the
   *  status band's mode block copies; the rose-pine dawn pair is read from LazyVim's lualine,
   *  the others are each scheme's own lualine theme. */
  status: { normal: [string, string]; insert: [string, string]; visual: [string, string] };
};

const palettes: Record<string, Palette> = {
  "tokyonight-night": {
    bg: "#1a1b26", fg: "#c0caf5", surface: "#16161e", surfaceFg: "#c0caf5", muted: "#7a82ab", border: "#292e42",
    cursorline: "#283457", chrome: "#16161e", chromeFg: "#a9b1d6", accent: "#7aa2f7", warn: "#e0af68",
    error: "#f7768e", ok: "#73daca", browse: "#7aa2f7", input: "#9ece6a", hintBg: "#ff9e64", hintFg: "#1a1b26",
    search: "#3d59a1", light: false,
    syn: { keyword: "#9d7cd8", function: "#7aa2f7", string: "#9ece6a", type: "#2ac3de", comment: "#565f89",
      number: "#ff9e64", constant: "#ff9e64", variable: "#c0caf5", operator: "#89ddff", punctuation: "#a9b1d6",
      property: "#73daca", tag: "#f7768e" },
    status: { normal: ["#7aa2f7", "#15161e"], insert: ["#9ece6a", "#15161e"], visual: ["#bb9af7", "#15161e"] },
  },
  "gruvbox-dark": {
    bg: "#282828", fg: "#ebdbb2", surface: "#32302f", surfaceFg: "#ebdbb2", muted: "#a89984", border: "#3c3836",
    cursorline: "#504945", chrome: "#504945", chromeFg: "#ebdbb2", accent: "#b8bb26", warn: "#fabd2f",
    error: "#fb4934", ok: "#b8bb26", browse: "#b8bb26", input: "#b8bb26", hintBg: "#fe8019", hintFg: "#282828",
    search: "#665c54", light: false,
    syn: { keyword: "#fb4934", function: "#b8bb26", string: "#b8bb26", type: "#fabd2f", comment: "#928374",
      number: "#d3869b", constant: "#d3869b", variable: "#83a598", operator: "#fe8019", punctuation: "#ebdbb2",
      property: "#83a598", tag: "#8ec07c" },
    status: { normal: ["#a89984", "#282828"], insert: ["#83a598", "#282828"], visual: ["#fe8019", "#282828"] },
  },
  "catppuccin-latte": {
    bg: "#eff1f5", fg: "#4c4f69", surface: "#e6e9ef", surfaceFg: "#4c4f69", muted: "#6c6f85", border: "#ccd0da",
    cursorline: "#ccd0da", chrome: "#e6e9ef", chromeFg: "#4c4f69", accent: "#1e66f5", warn: "#a86a0e",
    error: "#d20f39", ok: "#2f7d1f", browse: "#1e66f5", input: "#2f7d1f", hintBg: "#fe640b", hintFg: "#000000",
    search: "#bcdcea", light: true,
    syn: { keyword: "#8839ef", function: "#1e66f5", string: "#40a02b", type: "#df8e1d", comment: "#9ca0b0",
      number: "#fe640b", constant: "#fe640b", variable: "#4c4f69", operator: "#04a5e5", punctuation: "#7c7f93",
      property: "#1e66f5", tag: "#7287fd" },
    status: { normal: ["#1e66f5", "#dce0e8"], insert: ["#40a02b", "#dce0e8"], visual: ["#8839ef", "#dce0e8"] },
  },
  "rose-pine-dawn": {
    bg: "#faf4ed", fg: "#575279", surface: "#fffaf3", surfaceFg: "#575279", muted: "#797593", border: "#dfdad9",
    cursorline: "#e8e1dc", chrome: "#f2e9e1", chromeFg: "#575279", accent: "#b4637a", warn: "#b57614",
    error: "#b4637a", ok: "#286983", browse: "#d7827e", input: "#b57614", hintBg: "#d7827e", hintFg: "#000000",
    search: "#f2e9e1", light: true,
    syn: { keyword: "#286983", function: "#d7827e", string: "#ea9d34", type: "#56949f", comment: "#9893a5",
      number: "#ea9d34", constant: "#ea9d34", variable: "#575279", operator: "#797593", punctuation: "#797593",
      property: "#907aa9", tag: "#56949f" },
    status: { normal: ["#d7827e", "#faf4ed"], insert: ["#56949f", "#faf4ed"], visual: ["#907aa9", "#faf4ed"] },
  },
};

export const themeNames = Object.keys(palettes);

// The three tints and their text pairs, derived the way `tokens.rs` derives them (`tinted` in
// `ThemeTokens::derive`): the same sRGB mix with the same rounding, the same 5% steps toward the
// scheme's own colour and then a black/white pole, so the preview shows what the panel is sent.
type Rgb = [number, number, number];
const hexToRgb = (h: string): Rgb => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16)) as Rgb;
const rgbToHex = (c: Rgb): string => "#" + c.map((v) => v.toString(16).padStart(2, "0")).join("");
const mix = (a: Rgb, b: Rgb, t: number): Rgb => a.map((x, i) => Math.round(x + (b[i] - x) * t)) as Rgb;
function luminance(c: Rgb): number {
  const [r, g, b] = c.map((v) => {
    const s = v / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a: Rgb, b: Rgb): number {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}
function ensureContrast(color: Rgb, against: Rgb, toward: Rgb, min: number): Rgb {
  for (let step = 0; step <= 20; step += 1) {
    const candidate = mix(color, toward, step * 0.05);
    if (contrast(candidate, against) >= min) return candidate;
  }
  return toward;
}
// `ensure_contrast_against_both`: a colour that must hold `min` against two backgrounds at once. It tries the
// walk toward `toward`, then toward whichever pole serves both backgrounds better, then the other pole, and
// when nothing reaches `min` it keeps the step whose worse contrast is highest.
function ensureContrastAgainstBoth(color: Rgb, a: Rgb, b: Rgb, toward: Rgb, min: number): Rgb {
  const worse = (c: Rgb): number => Math.min(contrast(c, a), contrast(c, b));
  const [black, white]: [Rgb, Rgb] = [[0, 0, 0], [255, 255, 255]];
  const poles = worse(black) >= worse(white) ? [black, white] : [white, black];
  let best = color;
  for (const target of [toward, ...poles]) {
    for (let step = 0; step <= 20; step += 1) {
      const candidate = mix(color, target, step * 0.05);
      if (worse(candidate) >= min) return candidate;
      if (worse(candidate) > worse(best)) best = candidate;
    }
  }
  return best;
}
function ensureContrastWithPole(color: Rgb, against: Rgb, toward: Rgb, min: number): Rgb {
  const guarded = ensureContrast(color, against, toward, min);
  if (contrast(guarded, against) >= min) return guarded;
  const pole: Rgb = contrast(against, [0, 0, 0]) >= contrast(against, [255, 255, 255]) ? [0, 0, 0] : [255, 255, 255];
  return ensureContrast(color, against, pole, min);
}
function tinted(p: Palette, toward: string, amount: number): [string, string, string] {
  const [bg, fg, muted] = [hexToRgb(p.bg), hexToRgb(p.fg), hexToRgb(p.muted)];
  const fill = mix(bg, hexToRgb(toward), amount);
  const textToward = contrast(fill, bg) >= contrast(fill, fg) ? bg : fg;
  const text = ensureContrastWithPole(fg, fill, textToward, 4.5);
  return [rgbToHex(fill), rgbToHex(text), rgbToHex(ensureContrastWithPole(muted, fill, text, 4.5))];
}

// The cursor band's two fills and the composer's focus ring, derived the way `tokens.rs` derives them
// (`cursor_band_fill`, `focus_ring`): bg mixed toward cursorline in 2% steps from a light start until the
// fill reaches its minimum against bg; INPUT's colour held to 3:1 against bg.
function cursorBandFill(bg: Rgb, cursorline: Rgb, start: number, min: number): Rgb {
  for (let amount = start; amount < 1; amount += 0.02) {
    const candidate = mix(bg, cursorline, amount);
    if (contrast(candidate, bg) >= min) return candidate;
  }
  return cursorline;
}

/** `warn`/`error` as the panel is sent them (`band_signal`): held to 3:1 against `bg`, then against `bg` and
 *  `chrome` together, because the status band draws them as glyphs on `chrome`. */
function bandSignal(p: Palette, raw: string): string {
  const [bg, chrome, fg] = [hexToRgb(p.bg), hexToRgb(p.chrome), hexToRgb(p.fg)];
  return rgbToHex(ensureContrastAgainstBoth(ensureContrast(hexToRgb(raw), bg, fg, 3), bg, chrome, fg, 3));
}

export function themeVars(name: string): Record<string, string> {
  const base = palettes[name] ?? palettes["tokyonight-night"];
  // Everything below reads the guarded pair, the tint inputs included, as `ThemeTokens::derive` does.
  const p: Palette = { ...base, warn: bandSignal(base, base.warn), error: bandSignal(base, base.error) };
  const vars: Record<string, string> = {
    bg: p.bg, fg: p.fg, surface: p.surface, "surface-fg": p.surfaceFg, "surface-muted": p.muted,
    chrome: p.chrome, "chrome-fg": p.chromeFg, "chrome-muted": p.muted, "chrome-accent": p.accent,
    border: p.border, muted: p.muted, cursorline: p.cursorline, "hint-bg": p.hintBg, "hint-fg": p.hintFg,
    search: p.search, warn: p.warn, error: p.error, ok: p.ok, "mode-browse": p.browse, "mode-input": p.input,
  };
  for (const [prefix, toward, amount] of [["tint", p.fg, 0.07], ["warn-tint", p.warn, 0.06], ["error-tint", p.error, 0.09]] as const) {
    const [fill, text, secondary] = tinted(p, toward, amount);
    vars[prefix] = fill;
    vars[`${prefix}-fg`] = text;
    vars[`${prefix}-muted`] = secondary;
  }
  const [bg, fg, line] = [hexToRgb(p.bg), hexToRgb(p.fg), hexToRgb(p.cursorline)];
  vars["cursor-band"] = rgbToHex(cursorBandFill(bg, line, 0.24, 1.08));
  vars["cursor-band-inactive"] = rgbToHex(cursorBandFill(bg, line, 0.12, 1.035));
  vars["focus-ring"] = rgbToHex(ensureContrastWithPole(hexToRgb(p.status.insert[0]), bg, fg, 3));
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(vars)) out[`--nv-${k}`] = v;
  for (const [k, v] of Object.entries(p.syn)) out[`--nv-syn-${k}`] = v;
  for (const [mode, [bg, fg]] of Object.entries(p.status)) {
    out[`--nv-status-${mode}-bg`] = bg;
    out[`--nv-status-${mode}-fg`] = fg;
  }
  // `core/src/theme/tokens.rs`'s `PROSE_FONT_STACK`, which the real panel is sent.
  out["--nv-font-prose"] = '"IBM Plex Sans", "Adwaita Sans", Inter, system-ui, "Noto Sans CJK SC", sans-serif';
  out["--nv-font-mono"] = '"FiraCode Nerd Font", monospace';
  out["--nv-font-size"] = "14px";
  out["--nv-color-scheme"] = p.light ? "light" : "dark";
  return out;
}
