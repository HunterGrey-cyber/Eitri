//! The complete token set the window is painted with, derived from nvim's highlight groups.
//!
//! **This is the only place a fallback is decided.** The WebView and the GTK stylesheet both
//! receive a complete set, so neither ever needs a default of its own.

use super::color::{ensure_contrast, ensure_contrast_against_both, ensure_contrast_with_pole, Rgb};
use super::payload::{HlAttrs, NvimThemePayload};

/// nvim's own built-in default colorscheme (`NvimDarkGrey2` / `NvimLightGrey2`). Used only when
/// nvim has sent no `Normal` at all. Not the UI protocol's `default_colors_set`: that event is
/// consumed inside the neovide fork's private renderer and is not reachable from `shell`.
const DEFAULT_DARK_BG: Rgb = Rgb::new(0x14, 0x16, 0x1b);
const DEFAULT_DARK_FG: Rgb = Rgb::new(0xe0, 0xe2, 0xea);

const FALLBACK_WARN: Rgb = Rgb::new(0xd1, 0x9a, 0x00);
const FALLBACK_ERROR: Rgb = Rgb::new(0xd4, 0x38, 0x3a);
const FALLBACK_OK: Rgb = Rgb::new(0x3a, 0x8a, 0x3a);

/// Text must read (WCAG 1.4.3).
const TEXT_CONTRAST: f64 = 4.5;
/// Signs, rules and dots must be distinguishable (WCAG 1.4.11). 4.5 here would wash a scheme's
/// accent colours toward its foreground. **A token guarded only to this is not a text colour**: as
/// 12px text on rose-pine dawn, `warn` measured 3.02:1 and `error` 3.84:1.
const UI_CONTRAST: f64 = 3.0;
/// Only needs to be visibly a band. Not a WCAG figure.
const CURSORLINE_CONTRAST: f64 = 1.15;

pub const PROSE_FONT_STACK: &str = "\"IBM Plex Sans\", \"Noto Sans CJK SC\", sans-serif";
/// Neovide's embedded default face, which is what the editor draws with when `guifont` is empty.
const MONO_FONT_FALLBACK: &str = "\"FiraCode Nerd Font\", monospace";

/// `(token suffix, Treesitter group, legacy group)`. The Treesitter group wins when both are set.
const SYNTAX: [(&str, &str, &str); 12] = [
    ("keyword", "@keyword", "Statement"),
    ("function", "@function", "Function"),
    ("string", "@string", "String"),
    ("type", "@type", "Type"),
    ("comment", "@comment", "Comment"),
    ("number", "@number", "Number"),
    ("constant", "@constant", "Constant"),
    ("variable", "@variable", "Identifier"),
    ("operator", "@operator", "Operator"),
    ("punctuation", "@punctuation", "Delimiter"),
    ("property", "@property", "Identifier"),
    ("tag", "@tag", "Tag"),
];

/// Every group [`ThemeTokens::derive`] reads. `nvim_theme.lua` must request each one;
/// `crate::theme::feed`'s `the_lua_snippet_requests_every_group_derivation_reads` checks that.
/// **Add a name here in the same edit that makes `derive` read it.**
///
/// `#[cfg(test)]` again, and the round trip is worth recording. It was gated when its reader was a
/// test in `shell` and this constant was in `shell` too. L2 T1 (2026-09-17) moved the constant into
/// this crate and left the reader behind, which forced the gate off: `#[cfg(test)]` only compiles an
/// item in when the crate carrying it is itself built as a test target, not when it is a dependency
/// of another crate's tests, so a gated constant here would simply not have existed from `shell`'s
/// side. L2 T5 moved the reader -- and `nvim_theme.lua`, the file it asserts about -- into this
/// crate as well, so the contract is once more entirely inside one crate and the gate costs nothing.
///
/// `pub(crate)`, not `pub`: under `#[cfg(test)]` a `pub` here advertises an item that cannot exist
/// in any consumer's build, which is a worse thing to read than a narrower one.
#[cfg(test)]
pub(crate) const GROUPS_READ: &[&str] = &[
    "Normal",
    "NormalFloat",
    "Pmenu",
    "StatusLine",
    "WinSeparator",
    "VertSplit",
    "Comment",
    "Visual",
    "IncSearch",
    "Search",
    "DiagnosticWarn",
    "WarningMsg",
    "DiagnosticError",
    "ErrorMsg",
    "DiagnosticOk",
    "DiagnosticInfo",
    "Function",
    "String",
    "@keyword",
    "Statement",
    "@function",
    "@string",
    "@type",
    "Type",
    "@comment",
    "@number",
    "Number",
    "@constant",
    "Constant",
    "@variable",
    "Identifier",
    "@operator",
    "Operator",
    "@punctuation",
    "Delimiter",
    "@property",
    "@tag",
    "Tag",
];

#[derive(Clone, Debug, PartialEq)]
pub struct ThemeTokens {
    pub bg: Rgb,
    pub fg: Rgb,
    pub surface: Rgb,
    /// Text drawn on `surface` (sw-theme-2): the global `fg` is guarded against `bg`, not against
    /// `surface`, and a scheme whose floating background is not close to `bg` -- the shipped
    /// `zaibatsu`'s `NormalFloat` is `fg=#0e0024/bg=#ffffff` against a `Normal` of
    /// `fg=#ffffff/bg=#0e0024` -- can make `fg` on `surface` unreadable (1.0:1, measured). Derived
    /// the way `chrome_fg` is derived from `StatusLine`: `NormalFloat`/`Pmenu`'s own foreground,
    /// guarded toward whichever of `bg`/`fg` reads better against `surface`, with the same
    /// black/white pole fallback `hint_fg` has for when neither does.
    pub surface_fg: Rgb,
    /// Secondary text drawn on `surface` (a card's description, a diff gutter, the which-key box's
    /// footer): `muted` is guarded against `bg` and `cursorline`, never against `surface`, so under
    /// `zaibatsu` it is as unreadable on a card as `fg` is. Derived the way `chrome_muted` is: the
    /// Comment colour guarded against `surface`, toward `surface_fg`, with the pole fallback.
    pub surface_muted: Rgb,
    pub chrome: Rgb,
    pub chrome_fg: Rgb,
    /// Secondary text drawn on `chrome`. Guarded against `chrome`, not `bg`: a reversed StatusLine
    /// makes `chrome` Normal's fg, the very colour `muted` is pushed toward.
    pub chrome_muted: Rgb,
    /// Accent drawn on `chrome` -- the status bar's mode block. Guarded at `UI_CONTRAST` (3:1,
    /// WCAG 1.4.11's NON-text threshold), not at `TEXT_CONTRAST`, and this crate's own test
    /// asserts only `>= 3.0` for it. So it belongs on a border, a rule or a fill beside a label,
    /// never on the label's own glyphs, the same way `warn`/`error`/`ok` do below.
    ///
    /// Said in those words because it did not used to be: this line read "Accent TEXT drawn on
    /// chrome", which invited exactly the use its guard cannot support, and `index.css` ended up
    /// carrying a paragraph about why it does not follow the doc. `indexCss.test.ts` refuses this
    /// token as a `color:` even inside the winbar, which is the mechanism; this is the sign.
    pub chrome_accent: Rgb,
    pub border: Rgb,
    pub muted: Rgb,
    pub cursorline: Rgb,
    pub hint_bg: Rgb,
    pub hint_fg: Rgb,
    pub search: Rgb,
    /// `warn`/`error`/`ok` are guarded at `UI_CONTRAST` against `bg`: borders, rules, tints and
    /// dots only, never text. The panel draws warning and error text in `fg` and puts these on a
    /// border beside it; `agent-ui/web/src/indexCss.test.ts` holds its `color:` declarations to
    /// `fg`/`muted`. A consumer that needs coloured text needs its own 4.5:1 token, not these.
    pub warn: Rgb,
    pub error: Rgb,
    pub ok: Rgb,
    /// `mode_browse`/`mode_input` are a scheme's `Function`/`String` foregrounds and are **not
    /// guarded at all**: borders and fills only. Rose-pine dawn's `Function` is 2.60:1 on its `bg`.
    pub mode_browse: Rgb,
    pub mode_input: Rgb,
    /// `(suffix, colour)` in `SYNTAX` order.
    pub syntax: Vec<(&'static str, Rgb)>,
    /// A CSS `font-family` value.
    pub font_mono: String,
    /// The agent panel's base text size, in CSS pixels, and the ONE number every other size in
    /// `index.css` is a ratio of (2026-09-21). It is not derived from the payload: nvim's own
    /// `guifont` height is the editor's size for a monospace face, and the panel is mostly
    /// proportional prose, so following it 1:1 would make the two panes disagree by however much
    /// the two faces disagree. This is a host setting -- `init.lua`'s
    /// `eitri.config.set("agent.font_size", ...)` -- that `shell` writes over the default after
    /// deriving the rest. [`DEFAULT_PANEL_FONT_SIZE_PX`] is what the panel shipped with, so an
    /// unset key changes nothing.
    pub font_size_px: f32,
    /// `"light"` or `"dark"`, for CSS `color-scheme`.
    pub color_scheme: &'static str,
    /// The editor's own cell height, in the panel's CSS px, set by `shell` from `neovide-editor`'s
    /// `connect_cell_size_changed` -- which reports GTK logical px, a different unit whenever
    /// WebKitGTK zooms the page for the desktop's text scaling (`shell/src/webkit_zoom.rs`
    /// converts) -- `None` until nvim has reported a font (wave 4, R5: "agent
    /// pane最下面的input >> auto那一行太宽了，最好做到和旁边neovim底下的status一样宽"). Carried
    /// through the `ready` handshake and a reload the same way `font_size_px` is, with no new
    /// envelope: `--nv-editor-row` only appears in `css_vars()` when this is `Some`, so a reload
    /// before the editor's first frame gets `index.css`'s own `24px` fallback rather than a
    /// missing variable.
    pub editor_row_px: Option<f32>,
}

/// The `font-size` `index.css` was written against, and what `--nv-font-size` is unless a host says
/// otherwise. Changing this number rescales the whole panel; every other size there is a ratio of
/// it, so the proportions hold.
pub const DEFAULT_PANEL_FONT_SIZE_PX: f32 = 14.0;

/// The range `shell` accepts for `agent.font_size`. Not taste -- arithmetic: below the floor the
/// 0.786 ratio in `index.css`'s smallest step rounds to under 7px, and above the ceiling a single
/// tool-result line stops fitting a narrow panel. A value outside it is a startup failure that
/// names the key, never a silent clamp: silently ignoring a number someone typed is how a knob
/// gets reported as broken.
pub const PANEL_FONT_SIZE_RANGE_PX: std::ops::RangeInclusive<f32> = 9.0..=32.0;

/// Group lookups that understand `reverse` and fall through a list of names.
struct Groups<'a> {
    payload: &'a NvimThemePayload,
    bg: Rgb,
    fg: Rgb,
}

impl Groups<'_> {
    /// `(fg, bg)` as the group is actually drawn. A reversed group draws its own `bg` (or Normal's)
    /// as foreground and its own `fg` (or Normal's) as background -- many schemes define
    /// `IncSearch`/`Visual` as nothing but `reverse`.
    fn effective(&self, attrs: &HlAttrs) -> (Option<Rgb>, Option<Rgb>) {
        let fg = attrs.fg.map(Rgb::from_u32);
        let bg = attrs.bg.map(Rgb::from_u32);
        if attrs.reverse {
            (Some(bg.unwrap_or(self.bg)), Some(fg.unwrap_or(self.fg)))
        } else {
            (fg, bg)
        }
    }

    fn fg_of(&self, names: &[&str]) -> Option<Rgb> {
        names
            .iter()
            .filter_map(|n| self.payload.groups.get(*n))
            .find_map(|a| self.effective(a).0)
    }

    fn bg_of(&self, names: &[&str]) -> Option<Rgb> {
        names
            .iter()
            .filter_map(|n| self.payload.groups.get(*n))
            .find_map(|a| self.effective(a).1)
    }
}

impl ThemeTokens {
    pub fn fallback() -> Self {
        Self::derive(&NvimThemePayload::empty())
    }

    pub fn derive(payload: &NvimThemePayload) -> Self {
        let light = payload.options.background == "light";
        let (default_bg, default_fg) = if light {
            (DEFAULT_DARK_FG, DEFAULT_DARK_BG)
        } else {
            (DEFAULT_DARK_BG, DEFAULT_DARK_FG)
        };
        let normal = payload.groups.get("Normal");
        let bg = normal.and_then(|a| a.bg).map(Rgb::from_u32).unwrap_or(default_bg);
        let fg = normal.and_then(|a| a.fg).map(Rgb::from_u32).unwrap_or(default_fg);
        let g = Groups { payload, bg, fg };

        let surface = g.bg_of(&["NormalFloat", "Pmenu"]).unwrap_or_else(|| bg.mix(fg, 0.04));
        let hint_bg = g.bg_of(&["IncSearch"]).unwrap_or(fg);
        let hint_fg_raw = g.fg_of(&["IncSearch"]).unwrap_or(bg);
        // Toward whichever of Normal's two colours stands out more against the label background.
        let hint_toward = if hint_bg.contrast(bg) >= hint_bg.contrast(fg) {
            bg
        } else {
            fg
        };
        // Normal's two colours cannot always carry text on a mid-luminance IncSearch: on rose-pine
        // dawn's real `#d7827e`, the best either reaches is 2.60:1. `ensure_contrast_with_pole`
        // falls through to a black/white pole for exactly this case; see its own doc.
        let hint_fg = ensure_contrast_with_pole(hint_fg_raw, hint_bg, hint_toward, TEXT_CONTRAST);
        let signal =
            |names: &[&str], fallback: Rgb| ensure_contrast(g.fg_of(names).unwrap_or(fallback), bg, fg, UI_CONTRAST);
        let comment = g.fg_of(&["Comment"]);
        let function = g.fg_of(&["Function"]);

        // Text on surface is guarded against surface itself (sw-theme-2), toward whichever of
        // Normal's colours stands out more from it -- the same idiom `chrome_fg` uses against
        // `chrome` just below, with the same pole fallback `hint_fg` needs.
        let surface_toward = if surface.contrast(bg) >= surface.contrast(fg) {
            bg
        } else {
            fg
        };
        let surface_fg = ensure_contrast_with_pole(
            g.fg_of(&["NormalFloat", "Pmenu"]).unwrap_or(fg),
            surface,
            surface_toward,
            TEXT_CONTRAST,
        );

        // Text on chrome is guarded against chrome itself, toward whichever of Normal's colours
        // stands out more from it. Never against `bg`: a reversed StatusLine makes chrome Normal's fg.
        let chrome = g.bg_of(&["StatusLine"]).unwrap_or(surface);
        let chrome_toward = if chrome.contrast(bg) >= chrome.contrast(fg) {
            bg
        } else {
            fg
        };
        // sw-theme-4: `chrome_fg`/`chrome_muted` used to call plain `ensure_contrast`, which can
        // hand back a failing colour when neither of Normal's own colours reaches TEXT_CONTRAST
        // against `chrome` -- `hint_fg` already had the pole fallback these two were missing.
        let chrome_fg = ensure_contrast_with_pole(
            g.fg_of(&["StatusLine"]).unwrap_or(fg),
            chrome,
            chrome_toward,
            TEXT_CONTRAST,
        );

        // A reversed Visual draws the selection in inverse video; as a band under fg-coloured text
        // that would be the text colour itself. Only a Visual that paints its own background counts.
        // Two guards, readability last: visibly a band against bg, and fg text on it still reads.
        let visual_band = payload
            .groups
            .get("Visual")
            .filter(|a| !a.reverse)
            .and_then(|a| a.bg)
            .map(Rgb::from_u32);
        let cursorline = ensure_contrast(
            ensure_contrast(visual_band.unwrap_or(surface), bg, fg, CURSORLINE_CONTRAST),
            fg,
            bg,
            TEXT_CONTRAST,
        );

        // sw-theme-3: `muted` used to be guarded only against `bg`, though `index.css` also paints
        // it directly on a `cursorline` fill (the detail popover's and chooser's current row). It is
        // guarded against both at once: a second guard run after the first, toward `fg`, walked a
        // Comment colour sitting on the far side of `bg` from `fg` back across `bg` (whole-branch
        // review: black on a `#767676` bg became `#e6e6e6`, 3.64:1 there).
        let muted = ensure_contrast_against_both(
            comment.unwrap_or_else(|| bg.mix(fg, 0.5)),
            bg,
            cursorline,
            fg,
            TEXT_CONTRAST,
        );

        ThemeTokens {
            bg,
            fg,
            surface,
            surface_fg,
            surface_muted: ensure_contrast_with_pole(
                comment.unwrap_or_else(|| surface.mix(surface_fg, 0.5)),
                surface,
                surface_fg,
                TEXT_CONTRAST,
            ),
            chrome,
            chrome_fg,
            chrome_muted: ensure_contrast_with_pole(
                comment.unwrap_or_else(|| chrome.mix(chrome_fg, 0.5)),
                chrome,
                chrome_fg,
                TEXT_CONTRAST,
            ),
            chrome_accent: ensure_contrast(function.unwrap_or(chrome_fg), chrome, chrome_fg, UI_CONTRAST),
            border: g
                .fg_of(&["WinSeparator", "VertSplit"])
                .unwrap_or_else(|| bg.mix(fg, 0.15)),
            muted,
            cursorline,
            hint_bg,
            hint_fg,
            search: g.bg_of(&["Search"]).unwrap_or(hint_bg),
            warn: signal(&["DiagnosticWarn", "WarningMsg"], FALLBACK_WARN),
            error: signal(&["DiagnosticError", "ErrorMsg"], FALLBACK_ERROR),
            ok: signal(&["DiagnosticOk", "DiagnosticInfo"], FALLBACK_OK),
            mode_browse: function.unwrap_or(fg),
            mode_input: g.fg_of(&["String"]).unwrap_or(fg),
            syntax: SYNTAX
                .iter()
                .map(|&(name, ts, legacy)| (name, g.fg_of(&[ts, legacy]).unwrap_or(fg)))
                .collect(),
            font_mono: mono_font_stack(&payload.options.guifont),
            font_size_px: DEFAULT_PANEL_FONT_SIZE_PX,
            color_scheme: if light { "light" } else { "dark" },
            editor_row_px: None,
        }
    }

    pub fn css_vars(&self) -> Vec<(String, String)> {
        let colours = [
            ("bg", self.bg),
            ("fg", self.fg),
            ("surface", self.surface),
            ("surface-fg", self.surface_fg),
            ("surface-muted", self.surface_muted),
            ("chrome", self.chrome),
            ("chrome-fg", self.chrome_fg),
            ("chrome-muted", self.chrome_muted),
            ("chrome-accent", self.chrome_accent),
            ("border", self.border),
            ("muted", self.muted),
            ("cursorline", self.cursorline),
            ("hint-bg", self.hint_bg),
            ("hint-fg", self.hint_fg),
            ("search", self.search),
            ("warn", self.warn),
            ("error", self.error),
            ("ok", self.ok),
            ("mode-browse", self.mode_browse),
            ("mode-input", self.mode_input),
        ];
        let mut vars: Vec<(String, String)> = colours
            .into_iter()
            .map(|(name, colour)| (format!("--nv-{name}"), colour.hex()))
            .collect();
        vars.extend(
            self.syntax
                .iter()
                .map(|(name, colour)| (format!("--nv-syn-{name}"), colour.hex())),
        );
        vars.push(("--nv-font-prose".to_string(), PROSE_FONT_STACK.to_string()));
        vars.push(("--nv-font-mono".to_string(), self.font_mono.clone()));
        // `{}` on an `f32` prints `14`, not `14.0`, so the common case reads as it was typed.
        vars.push(("--nv-font-size".to_string(), format!("{}px", self.font_size_px)));
        vars.push(("--nv-color-scheme".to_string(), self.color_scheme.to_string()));
        if let Some(px) = self.editor_row_px {
            vars.push(("--nv-editor-row".to_string(), format!("{px}px")));
        }
        vars
    }
}

/// `guifont` as a CSS `font-family` list, parsed the way Neovide does (`font_options.rs`): options
/// start at the first `:`, families are comma-separated, and `_` or `\ ` stand for a space.
///
/// Characters that could end a CSS value or a quoted family (`" \ ; { } < >`) are dropped. The
/// value reaches the WebView through `setProperty`, which already refuses to parse across a
/// declaration, so this is defence in depth -- `guifont` is set by whatever runs inside nvim.
pub(crate) fn mono_font_stack(guifont: &str) -> String {
    let families = guifont.split(':').next().unwrap_or("");
    let mut stack: Vec<String> = families
        .split(',')
        .map(|f| f.replace("\\ ", " ").replace('_', " "))
        .map(|f| {
            f.chars()
                .filter(|c| !matches!(c, '"' | '\\' | ';' | '{' | '}' | '<' | '>'))
                .collect::<String>()
        })
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .map(|f| format!("\"{f}\""))
        .collect();
    stack.push(MONO_FONT_FALLBACK.to_string());
    stack.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};
    use std::collections::HashMap;

    fn hl(fg: Option<u32>, bg: Option<u32>) -> HlAttrs {
        HlAttrs { fg, bg, reverse: false }
    }

    fn payload(background: &str, groups: &[(&str, HlAttrs)]) -> NvimThemePayload {
        NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: groups
                .iter()
                .map(|(n, a)| (n.to_string(), a.clone()))
                .collect::<HashMap<_, _>>(),
            options: NvimOptions {
                background: background.into(),
                guifont: String::new(),
                colors_name: "t".into(),
            },
        }
    }

    #[test]
    fn the_fallback_is_nvims_own_default_dark_scheme_and_complete() {
        let t = ThemeTokens::fallback();
        assert_eq!(t.bg.hex(), "#14161b");
        assert_eq!(t.fg.hex(), "#e0e2ea");
        assert_eq!(t.color_scheme, "dark");
        let vars = t.css_vars();
        assert_eq!(vars.len(), 36);
        assert!(vars
            .iter()
            .all(|(name, value)| name.starts_with("--nv-") && !value.is_empty()));
        let names: std::collections::HashSet<_> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.len(), 36, "no duplicate variable names");
        // The count alone would be satisfied by any 36th variable, and this one carries a UNIT --
        // a bare `14` reaches the panel as an invalid `font-size` and every ratio built on it
        // silently falls back to the browser default.
        assert_eq!(
            vars.iter()
                .find(|(n, _)| n == "--nv-font-size")
                .map(|(_, v)| v.as_str()),
            Some("14px")
        );
    }

    /// The panel's own default, its accepted range, and the fact that a size is a plain number of
    /// CSS pixels -- three things `shell` validates against and `index.css` is written around.
    #[test]
    fn the_panel_font_size_is_a_host_setting_with_a_default_that_changes_nothing() {
        assert_eq!(DEFAULT_PANEL_FONT_SIZE_PX, 14.0);
        assert_eq!(ThemeTokens::fallback().font_size_px, DEFAULT_PANEL_FONT_SIZE_PX);
        assert!(PANEL_FONT_SIZE_RANGE_PX.contains(&DEFAULT_PANEL_FONT_SIZE_PX));
        assert!(!PANEL_FONT_SIZE_RANGE_PX.contains(&0.0));
        assert!(!PANEL_FONT_SIZE_RANGE_PX.contains(&100.0));

        // A host writes over the default after deriving, and only that one variable moves.
        let mut t = ThemeTokens::fallback();
        t.font_size_px = 16.5;
        let vars = t.css_vars();
        assert_eq!(
            vars.iter()
                .find(|(n, _)| n == "--nv-font-size")
                .map(|(_, v)| v.as_str()),
            Some("16.5px")
        );
        let unchanged = ThemeTokens::fallback().css_vars();
        let moved: Vec<&str> = vars
            .iter()
            .zip(unchanged.iter())
            .filter(|((_, a), (_, b))| a != b)
            .map(|((n, _), _)| n.as_str())
            .collect();
        assert_eq!(moved, vec!["--nv-font-size"]);
    }

    /// Wave 4, R5: absent, the panel keeps `index.css`'s own `24px` fallback; `shell` sets it the
    /// moment `neovide-editor`'s `connect_cell_size_changed` reports a cell height.
    #[test]
    fn the_editor_row_is_a_css_var_only_once_the_editor_reported_it() {
        let mut t = ThemeTokens::fallback();
        assert!(
            t.css_vars().iter().all(|(n, _)| n != "--nv-editor-row"),
            "absent: the CSS fallback applies"
        );
        t.editor_row_px = Some(22.0);
        assert!(t
            .css_vars()
            .contains(&("--nv-editor-row".to_string(), "22px".to_string())));
    }

    #[test]
    fn a_light_background_with_no_normal_uses_the_inverted_default() {
        let t = ThemeTokens::derive(&payload("light", &[]));
        assert_eq!(t.bg.hex(), "#e0e2ea");
        assert_eq!(t.fg.hex(), "#14161b");
        assert_eq!(t.color_scheme, "light");
    }

    #[test]
    fn rose_pine_dawn_keeps_its_colours_where_they_are_readable() {
        let t = ThemeTokens::derive(&payload(
            "light",
            &[
                ("Normal", hl(Some(0x575279), Some(0xfaf4ed))),
                ("NormalFloat", hl(Some(0x575279), Some(0xfffaf3))),
                ("Visual", hl(None, Some(0xdfdad9))),
                ("Comment", hl(Some(0x9893a5), None)),
                ("IncSearch", hl(Some(0xfaf4ed), Some(0xd7827e))),
            ],
        ));
        assert_eq!(t.surface.hex(), "#fffaf3");
        assert_eq!(
            t.cursorline.hex(),
            "#dfdad9",
            "1.27:1 already clears the 1.15 cursorline guard"
        );
        assert_eq!(t.hint_bg.hex(), "#d7827e");
        // Neither of Normal's colours reaches 4.5:1 on this IncSearch (2.56 and 2.60): the label
        // text must still read.
        assert!(t.hint_fg.contrast(t.hint_bg) >= 4.5, "{}", t.hint_fg.hex());
        // Comment is 2.73:1 on this background: guarded toward the foreground, not replaced by it.
        assert!(t.muted.contrast(t.bg) >= 4.5);
        assert_ne!(t.muted, Rgb::from_u32(0x9893a5));
        assert_ne!(t.muted, t.fg);
    }

    #[test]
    fn a_reversed_group_with_no_colours_draws_normal_inverted() {
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xffffff), Some(0x000000))),
                (
                    "IncSearch",
                    HlAttrs {
                        fg: None,
                        bg: None,
                        reverse: true,
                    },
                ),
            ],
        ));
        assert_eq!(t.hint_bg.hex(), "#ffffff");
        assert_eq!(t.hint_fg.hex(), "#000000");
    }

    #[test]
    fn missing_groups_fall_back_to_mixes_of_normal() {
        let bg = Rgb::from_u32(0x000000);
        let fg = Rgb::from_u32(0xffffff);
        let t = ThemeTokens::derive(&payload("dark", &[("Normal", hl(Some(0xffffff), Some(0x000000)))]));
        assert_eq!(t.surface, bg.mix(fg, 0.04));
        assert_eq!(t.chrome, t.surface);
        assert_eq!(t.border, bg.mix(fg, 0.15));
        assert_eq!(t.search, t.hint_bg);
        assert!(t.syntax.iter().all(|(_, c)| *c == fg));
    }

    #[test]
    fn the_treesitter_group_wins_over_the_legacy_one() {
        let both = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("@keyword", hl(Some(0x111111), None)),
                ("Statement", hl(Some(0x222222), None)),
            ],
        ));
        let legacy_only = ThemeTokens::derive(&payload("dark", &[("Statement", hl(Some(0x222222), None))]));
        let keyword = |t: &ThemeTokens| t.syntax.iter().find(|(n, _)| *n == "keyword").unwrap().1;
        assert_eq!(keyword(&both).hex(), "#111111");
        assert_eq!(keyword(&legacy_only).hex(), "#222222");
    }

    #[test]
    fn signal_colours_are_guarded_at_three_to_one() {
        // Near-white warning text on white: unusable as-is.
        let t = ThemeTokens::derive(&payload(
            "light",
            &[
                ("Normal", hl(Some(0x000000), Some(0xffffff))),
                ("DiagnosticWarn", hl(Some(0xfff8e0), None)),
            ],
        ));
        assert!(t.warn.contrast(t.bg) >= 3.0);
    }

    #[test]
    fn a_reverse_only_visual_never_becomes_the_text_colour() {
        // morhetz/gruvbox with g:gruvbox_invert_selection=1: Visual = {bg = bg3, reverse = true}.
        // Drawn as inverse video its "background" is Normal's fg -- the colour of the text a hovered
        // row carries.
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xebdbb2), Some(0x282828))),
                (
                    "Visual",
                    HlAttrs {
                        fg: None,
                        bg: Some(0x665c54),
                        reverse: true,
                    },
                ),
            ],
        ));
        assert_ne!(t.cursorline, t.fg);
        assert!(
            t.cursorline.contrast(t.fg) >= 4.5,
            "fg text on a hovered row must stay readable"
        );
        assert!(t.cursorline.contrast(t.bg) >= 1.15);
    }

    #[test]
    fn text_on_a_reversed_statusline_stays_readable() {
        // lunaperche (dark) and quiet (light) as nvim 0.12 ships them: StatusLine is reverse-only and
        // Function has no fg, so chrome becomes Normal's fg -- the colour bg-guarded tokens are pushed
        // toward. Every text token drawn on chrome must be guarded against chrome itself.
        for (background, normal, comment) in [
            ("dark", hl(Some(0xc6c6c6), Some(0x000000)), 0x949494),
            ("light", hl(Some(0x000000), Some(0xd7d7d7)), 0x000000),
        ] {
            let t = ThemeTokens::derive(&payload(
                background,
                &[
                    ("Normal", normal),
                    (
                        "StatusLine",
                        HlAttrs {
                            fg: None,
                            bg: None,
                            reverse: true,
                        },
                    ),
                    ("Comment", hl(Some(comment), None)),
                ],
            ));
            assert_eq!(
                t.chrome, t.fg,
                "{background}: a reversed StatusLine paints chrome with Normal's fg"
            );
            assert!(t.chrome_fg.contrast(t.chrome) >= 4.5, "{background}: chrome_fg");
            assert!(t.chrome_muted.contrast(t.chrome) >= 4.5, "{background}: chrome_muted");
            assert!(t.chrome_accent.contrast(t.chrome) >= 3.0, "{background}: chrome_accent");
        }
    }

    #[test]
    fn css_vars_carry_hex_colours_and_the_font_stacks() {
        let t = ThemeTokens::fallback();
        let vars: HashMap<_, _> = t.css_vars().into_iter().collect();
        assert_eq!(vars["--nv-bg"], "#14161b");
        assert_eq!(vars["--nv-font-prose"], PROSE_FONT_STACK);
        assert_eq!(vars["--nv-font-mono"], "\"FiraCode Nerd Font\", monospace");
        assert_eq!(vars["--nv-color-scheme"], "dark");
        assert!(vars.contains_key("--nv-syn-punctuation"));
    }

    #[test]
    fn guifont_is_parsed_the_way_neovide_parses_it() {
        assert_eq!(mono_font_stack(""), "\"FiraCode Nerd Font\", monospace");
        assert_eq!(
            mono_font_stack("Maple_Mono_NF_CN:h14"),
            "\"Maple Mono NF CN\", \"FiraCode Nerd Font\", monospace"
        );
        assert_eq!(
            mono_font_stack("Fira\\ Code,Symbols Nerd Font:h12:b"),
            "\"Fira Code\", \"Symbols Nerd Font\", \"FiraCode Nerd Font\", monospace"
        );
    }

    #[test]
    fn guifont_cannot_break_out_of_a_css_value() {
        assert_eq!(
            mono_font_stack("evil\";}body{x"),
            "\"evilbodyx\", \"FiraCode Nerd Font\", monospace"
        );
    }

    /// sw-theme-2's own probe: the shipped `zaibatsu` colorscheme's real highlight data (measured
    /// via headless nvim: `Normal` fg=#ffffff/bg=#0e0024, `NormalFloat` linking through `Pmenu` to
    /// fg=#0e0024/bg=#ffffff). `surface` picks up the floating background but nothing ever guards a
    /// foreground for text drawn on it, so a tool/permission card painted `--nv-surface` under
    /// `--nv-fg` text renders white on white.
    #[test]
    fn a_surface_guarded_foreground_stays_readable_on_zaibatsu() {
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xffffff), Some(0x0e0024))),
                ("NormalFloat", hl(Some(0x0e0024), Some(0xffffff))),
                ("Pmenu", hl(Some(0x0e0024), Some(0xffffff))),
            ],
        ));
        assert_eq!(t.surface.hex(), "#ffffff");
        assert_eq!(t.fg.hex(), "#ffffff");
        assert!(
            (t.surface.contrast(t.fg) - 1.0).abs() < 1e-9,
            "the bug: the global fg is unguarded against surface"
        );
        assert!(
            t.surface_fg.contrast(t.surface) >= TEXT_CONTRAST,
            "surface_fg must read against surface, got {}",
            t.surface_fg.contrast(t.surface)
        );
        assert!(
            t.surface_muted.contrast(t.surface) >= TEXT_CONTRAST,
            "surface_muted must read against surface, got {}",
            t.surface_muted.contrast(t.surface)
        );
    }

    /// sw-theme-3's own probe: the shipped `pablo` colorscheme's real highlight data (`Comment`
    /// fg=#808080, `Visual` bg=#a9a9a9 not-reverse, `Normal` fg=#ffffff/bg=#000000). `cursorline` is
    /// guarded only against `fg` and `muted` only against `bg` -- never against each other, though
    /// `index.css` paints `muted` text directly on a `cursorline` fill (the detail popover's current
    /// row, the chooser's current row).
    #[test]
    fn muted_text_stays_readable_on_the_real_cursorline_fill_on_pablo() {
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xffffff), Some(0x000000))),
                ("Comment", hl(Some(0x808080), None)),
                ("Visual", hl(None, Some(0xa9a9a9))),
            ],
        ));
        assert!(t.cursorline.contrast(t.fg) >= TEXT_CONTRAST, "cursorline's own guard");
        assert!(t.muted.contrast(t.bg) >= TEXT_CONTRAST, "muted's own guard");
        assert!(
            t.muted.contrast(t.cursorline) >= TEXT_CONTRAST,
            "muted must also read on cursorline, got {}",
            t.muted.contrast(t.cursorline)
        );
    }

    /// sw-theme-3, whole-branch review (Codex): the second stage that guards `muted` against
    /// `cursorline` mixed toward `fg` on the claim that this can only move it further from `bg`.
    /// That holds only while `muted` sits on `fg`'s side of `bg`. Here the Comment colour (black) is
    /// on the OTHER side of a mid-grey `bg` (4.62:1, passing the first stage untouched), so mixing
    /// it toward white to clear `cursorline` walks it back across `bg`: `#e6e6e6`, 4.60:1 on
    /// `cursorline` and 3.64:1 on `bg`. Muted text is painted on both, so it must read on both.
    #[test]
    fn muted_reads_on_bg_and_cursorline_at_once_when_the_comment_sits_across_bg() {
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xffffff), Some(0x767676))),
                ("Comment", hl(Some(0x000000), None)),
                ("Visual", hl(None, Some(0x666666))),
            ],
        ));
        assert!(
            t.muted.contrast(t.bg) >= TEXT_CONTRAST && t.muted.contrast(t.cursorline) >= TEXT_CONTRAST,
            "muted {} must read on bg {} ({:.2}) and on cursorline {} ({:.2})",
            t.muted.hex(),
            t.bg.hex(),
            t.muted.contrast(t.bg),
            t.cursorline.hex(),
            t.muted.contrast(t.cursorline)
        );
    }

    /// sw-theme-4's own probe: the real payload the verifier fed through `ThemeTokens::derive`
    /// (rose-pine dawn's `Normal` fg=#575279/bg=#faf4ed, `StatusLine` fg=#faf4ed/bg=#d7827e).
    /// Neither of Normal's own colours reaches 4.5:1 against this `chrome`, and `ensure_contrast`'s
    /// own doc says it "returns toward when no step does" -- so without a pole fallback, `chrome_fg`
    /// (and `chrome_muted`, which is guarded toward `chrome_fg`) is returned unchanged, below
    /// `TEXT_CONTRAST`. `hint_fg` in this same file already has exactly this fallback; chrome text
    /// needs the same guarantee.
    #[test]
    fn chrome_text_reaches_text_contrast_even_when_normals_colours_cannot_carry_it() {
        let t = ThemeTokens::derive(&payload(
            "light",
            &[
                ("Normal", hl(Some(0x575279), Some(0xfaf4ed))),
                ("StatusLine", hl(Some(0xfaf4ed), Some(0xd7827e))),
            ],
        ));
        assert_eq!(t.chrome.hex(), "#d7827e");
        assert!(
            t.chrome_fg.contrast(t.chrome) >= TEXT_CONTRAST,
            "chrome_fg must fall back to a pole, got {}",
            t.chrome_fg.contrast(t.chrome)
        );
        assert!(
            t.chrome_muted.contrast(t.chrome) >= TEXT_CONTRAST,
            "chrome_muted must fall back to a pole too, got {}",
            t.chrome_muted.contrast(t.chrome)
        );
    }
}
