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
/// An outline without the keys remains visible while receding below the active ring.
const FOCUS_RING_INACTIVE_CONTRAST: f64 = 1.6;
/// Only needs to be visibly a band. Not a WCAG figure.
const CURSORLINE_CONTRAST: f64 = 1.15;
/// The conversation's cursor band is the only mark of the cursor there (no bar, no outline), so its fill has
/// to be seen. It starts from a light mix of `cursorline` into `bg` and is raised
/// in 2% steps until it reaches this much contrast against `bg`:
/// the band while the panel holds the keys, and the weaker one while it does not. A plain 24%/12% mix
/// measured 1.02-1.06:1 on tokyonight and rose-pine dawn, which is no band at all. Not WCAG figures.
const CURSOR_BAND_CONTRAST: f64 = 1.08;
const CURSOR_BAND_INACTIVE_CONTRAST: f64 = 1.035;
const CURSOR_BAND_START: f64 = 0.24;
const CURSOR_BAND_INACTIVE_START: f64 = 0.12;

/// The panel's prose face, and the window chrome's. IBM Plex Sans where it is installed; otherwise the
/// desktop's own sans (Adwaita Sans on GNOME, Inter, then whatever `system-ui` names -- San Francisco on
/// macOS) rather than the CJK face's Latin letters, and Noto Sans CJK SC still before the generic family
/// so Chinese text never falls to a face without it.
pub const PROSE_FONT_STACK: &str =
    "\"IBM Plex Sans\", \"Adwaita Sans\", Inter, system-ui, \"Noto Sans CJK SC\", sans-serif";
/// Neovide's embedded default face, which is what the editor draws with when `guifont` is empty.
const MONO_FONT_FALLBACK: &str = "\"FiraCode Nerd Font\", monospace";
/// Maximum Unicode scalar values in one font family.
pub const FONT_FAMILY_MAX_CHARS: usize = 100;
/// Maximum families in the complete font list, including the fallback.
pub const FONT_LIST_MAX_FAMILIES: usize = 16;
/// Maximum Unicode scalar values in the complete font list, including the fallback.
pub const FONT_LIST_MAX_CHARS: usize = 1024;
/// Maximum user families before the fallback's two families.
pub const FONT_USER_MAX_FAMILIES: usize = 14;

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

/// The groups a Markdown link's text is drawn with in the editor, most specific first. A plugin page that shows
/// links in body text (the Markdown preview) colours them with the first of these that sets a foreground, so a link
/// looks as it does in the buffer beside it.
const LINK_GROUPS: [&str; 3] = ["@markup.link.label", "@markup.link", "Underlined"];

/// Every group [`ThemeTokens::derive`] reads. `nvim_theme.lua` must request each one;
/// `crate::theme::feed`'s `the_lua_snippet_requests_every_group_derivation_reads` checks that.
/// **Add a name here in the same edit that makes `derive` read it.**
///
/// `#[cfg(test)]` again, and the round trip is worth recording. It was gated when its reader was a
/// test in `shell` and this constant was in `shell` too. A move on 2026-09-17 took the constant into
/// this crate and left the reader behind, which forced the gate off: `#[cfg(test)]` only compiles an
/// item in when the crate carrying it is itself built as a test target, not when it is a dependency
/// of another crate's tests, so a gated constant here would simply not have existed from `shell`'s
/// side. A later move took the reader -- and `nvim_theme.lua`, the file it asserts about -- into this
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
    "@markup.link.label",
    "@markup.link",
    "Underlined",
    "lualine_a_normal",
    "lualine_a_insert",
    "lualine_a_visual",
];

#[derive(Clone, Debug, PartialEq)]
pub struct ThemeTokens {
    pub bg: Rgb,
    pub fg: Rgb,
    pub surface: Rgb,
    /// Text drawn on `surface`: the global `fg` is guarded against `bg`, not against
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
    /// `StatusLine`'s fill: what the status band continues from the editor's statusline, and nothing else. A
    /// plugin can repaint that group while nvim runs, so the window's own chrome stands on `surface` instead.
    pub chrome: Rgb,
    pub chrome_fg: Rgb,
    /// Secondary text drawn on `chrome`. Guarded against `chrome`, not `bg`: a reversed StatusLine
    /// makes `chrome` Normal's fg, the very colour `muted` is pushed toward.
    pub chrome_muted: Rgb,
    /// Accent drawn on `chrome` -- the status bar's mode block -- and on `bg`: the panel's state glyphs
    /// (the activity line's star, a running tool's `◐`, the chooser's unread dot). Guarded against both
    /// at once at `UI_CONTRAST` (3:1, WCAG 1.4.11's NON-text threshold), not at `TEXT_CONTRAST`, and
    /// this crate's own tests assert only `>= 3.0` for it. So it belongs on a border, a rule, a fill
    /// or a glyph, never on a label's own text, the same way `warn`/`error`/`ok` do below. (Until
    /// 2026-10-13 it was guarded against `chrome` alone, though the chat page already drew it on `bg`.)
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
    /// `warn`/`error` are guarded at `UI_CONTRAST` against both `bg` and `chrome` (the status band's
    /// mode glyphs sit on `chrome`), `ok` against `bg` alone: borders, rules, tints and
    /// dots only, never text. The panel draws warning and error text in `fg` and puts these on a
    /// border beside it; `agent-ui/web/src/indexCss.test.ts` holds its `color:` declarations to
    /// `fg`/`muted`. A consumer that needs coloured text needs its own 4.5:1 token, not these.
    pub warn: Rgb,
    pub error: Rgb,
    pub ok: Rgb,
    /// The panel's three tinted fills, each a small step from `bg`, each with its own text pair. `tint`
    /// (7% toward `fg`) is the surface that is yours: your sent messages, the composer, an inline code
    /// span. `warn_tint` (6% toward `warn`) is a waiting permission card. `error_tint` (9% toward
    /// `error`) is a lost session. Mixed here, not in CSS, so the text is
    /// guarded against the very colour painted.
    ///
    /// `fg`/`muted` are guarded against `bg` only, and a tint spends some of that margin (measured on
    /// the preview's four palettes: `muted` on `tint` 3.64-4.44:1, on the warn and error tints
    /// 3.63-4.83:1). So each tint's `*_fg` is `fg` guarded against that tint and its `*_muted` is
    /// `muted` guarded toward its `*_fg`, both at `TEXT_CONTRAST` with the black/white pole fallback,
    /// the way `surface_fg`/`surface_muted` are for `surface`. One pair per tint, not one for all three:
    /// on a mid-grey `bg` the `fg` tint and the `error` tint can fall on opposite sides of mid-grey,
    /// where no single colour reads on both (white on `#767676`'s `fg` tint: 3.95:1). On most schemes
    /// each `*_fg` is `fg` itself and each `*_muted` is `muted` or a step past it.
    pub tint: Rgb,
    pub tint_fg: Rgb,
    pub tint_muted: Rgb,
    pub warn_tint: Rgb,
    pub warn_tint_fg: Rgb,
    pub warn_tint_muted: Rgb,
    pub error_tint: Rgb,
    pub error_tint_fg: Rgb,
    pub error_tint_muted: Rgb,
    /// `mode_browse`/`mode_input` are a scheme's `Function`/`String` foregrounds and are **not
    /// guarded at all**: borders and fills only. Rose-pine dawn's `Function` is 2.60:1 on its `bg`.
    pub mode_browse: Rgb,
    pub mode_input: Rgb,
    /// The panel's mode block in the status band, drawn as the editor's statusline draws its own mode
    /// section beside it: lualine's `lualine_a_normal`/`_insert`/`_visual` pairs, `(bg, fg)`, copied as
    /// they are. **The fg is deliberately not guarded**, for the reason `syntax` is not: it is the
    /// editor's own colour, which the editor draws with the same pair a few pixels to the left, and a
    /// guard would make the two blocks differ, which is the one thing this pair is for (rose-pine dawn's
    /// normal block is 2.5:1, in the editor and here alike). With no lualine -- the group missing, or
    /// either colour unset -- each falls back to the block the panel drew before, `cursorline` under
    /// `fg`, a pair this crate does guard.
    pub status_normal: (Rgb, Rgb),
    pub status_insert: (Rgb, Rgb),
    pub status_visual: (Rgb, Rgb),
    /// The fill of the conversation's cursor band while the panel holds the keys, and the weaker one
    /// while it does not: `bg` mixed toward `cursorline` from a light start until the fill reaches
    /// `CURSOR_BAND_CONTRAST` / `CURSOR_BAND_INACTIVE_CONTRAST` against `bg`, never past `cursorline`
    /// itself. Both are therefore between `bg` and `cursorline`, so the text `cursorline` was guarded
    /// for still reads on them.
    pub cursor_band: Rgb,
    pub cursor_band_inactive: Rgb,
    /// The ring a focused composer draws, guarded at `UI_CONTRAST` against `bg` (the composer's own
    /// fill). INPUT's colour, from lualine's insert section where there is one: that block's `bg` is
    /// the editor's own colour and unguarded, and where it is close to `bg` (or is the `cursorline`
    /// stand-in, 1.15:1) a ring drawn in it is fainter than the unfocused frame. Without lualine it
    /// starts from `String`'s foreground, the colour `mode_input` already names. A border, outline or
    /// fill only, like `warn`; never text.
    pub focus_ring: Rgb,
    /// A light mix of the active ring into the page, raised only until its outline is visible.
    pub focus_ring_inactive: Rgb,
    /// Link text in a page beside the editor: the editor's own Markdown link colour ([`LINK_GROUPS`]), else the
    /// colour `--nv-syn-function` has. **Not guarded**, for the reason `syntax` is not: it is the colour the editor
    /// draws the same link with a few pixels to the left, and a guard would make the two differ. It is body text,
    /// which the signal colours (`warn`, `chrome_accent`, ...) must never be, and `syntax` means code.
    pub link: Rgb,
    /// `(suffix, colour)` in `SYNTAX` order.
    pub syntax: Vec<(&'static str, Rgb)>,
    /// A CSS `font-family` value.
    pub font_mono: String,
    /// The agent panel's base text size, in CSS pixels, and the ONE number every other size in
    /// `index.css` is a ratio of (2026-09-21). A host setting `shell` writes over the default after
    /// deriving the rest: `init.lua`'s `eitri.config.set("agent.font_size", ...)` when it is set, and
    /// otherwise (since 2026-10-15) the editor's own text size, [`guifont_size_px`] converted out of
    /// WebKit's page zoom, so the two panes beside each other are one size. A reply's prose and the
    /// panel's mono content are drawn at this size itself. [`DEFAULT_PANEL_FONT_SIZE_PX`] is what a
    /// host with neither (a companion window, the macOS host) starts from.
    pub font_size_px: f32,
    /// `"light"` or `"dark"`, for CSS `color-scheme`.
    pub color_scheme: &'static str,
    /// The editor's own cell height, in the panel's CSS px, set by `shell` from `neovide-editor`'s
    /// `connect_cell_size_changed` -- which reports GTK logical px, a different unit whenever
    /// WebKitGTK zooms the page for the desktop's text scaling (`shell/src/webkit_zoom.rs`
    /// converts) -- `None` until nvim has reported a font, so the composer's mode line can be sized to
    /// match the editor's status line beside it. Carried
    /// through the `ready` handshake and a reload the same way `font_size_px` is, with no new
    /// envelope: `--nv-editor-row` only appears in `css_vars()` when this is `Some`, so a reload
    /// before the editor's first frame gets `index.css`'s own `24px` fallback rather than a
    /// missing variable.
    pub editor_row_px: Option<f32>,
    /// nvim's `'cmdheight'`, the rows the editor keeps under its statusline, so the panel's status band
    /// can sit on the statusline's row instead of the command line's. Sent as `--nv-editor-cmd-rows`
    /// only beside an editor whose row height is known ([`Self::editor_row_px`]): a window with no
    /// editor under the band (a companion window) has no row to line up with.
    pub editor_cmd_rows: Option<u32>,
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

        // Text on surface is guarded against surface itself, toward whichever of
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
        // Plain `ensure_contrast` can hand back a failing colour when neither of Normal's own colours
        // reaches TEXT_CONTRAST against `chrome`, so `chrome_fg` and `chrome_muted` take the same pole
        // fallback `hint_fg` uses.
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

        // `muted` used to be guarded only against `bg`, though `index.css` also paints
        // it directly on a `cursorline` fill (the detail popover's and chooser's current row). It is
        // guarded against both at once: a second guard run after the first, toward `fg`, walked a
        // Comment colour sitting on the far side of `bg` from `fg` back across `bg` (black on a
        // `#767676` bg became `#e6e6e6`, 3.64:1 there).
        let muted = ensure_contrast_against_both(
            comment.unwrap_or_else(|| bg.mix(fg, 0.5)),
            bg,
            cursorline,
            fg,
            TEXT_CONTRAST,
        );

        // `warn` and `error` are also the colours of the status band's `auto` and `bypass` mode glyphs,
        // which sit on `chrome`, so they are guarded against `chrome` as well as `bg`. Where `chrome`
        // is close to `bg` this changes nothing: a colour that already passes both is returned as is.
        let band_signal = |names: &[&str], fallback: Rgb| {
            let raw = g.fg_of(names).unwrap_or(fallback);
            ensure_contrast_against_both(ensure_contrast(raw, bg, fg, UI_CONTRAST), bg, chrome, fg, UI_CONTRAST)
        };
        let warn = band_signal(&["DiagnosticWarn", "WarningMsg"], FALLBACK_WARN);
        let error = band_signal(&["DiagnosticError", "ErrorMsg"], FALLBACK_ERROR);
        // The three tinted fills and the pair of text colours on each (see the fields' own doc).
        let tinted = |toward: Rgb, amount: f64| -> (Rgb, Rgb, Rgb) {
            let fill = bg.mix(toward, amount);
            let text_toward = if fill.contrast(bg) >= fill.contrast(fg) { bg } else { fg };
            let text = ensure_contrast_with_pole(fg, fill, text_toward, TEXT_CONTRAST);
            let secondary = ensure_contrast_with_pole(muted, fill, text, TEXT_CONTRAST);
            (fill, text, secondary)
        };
        let (tint, tint_fg, tint_muted) = tinted(fg, 0.07);
        let (warn_tint, warn_tint_fg, warn_tint_muted) = tinted(warn, 0.06);
        let (error_tint, error_tint_fg, error_tint_muted) = tinted(error, 0.09);

        let cursor_band = cursor_band_fill(bg, cursorline, CURSOR_BAND_START, CURSOR_BAND_CONTRAST);
        let cursor_band_inactive = cursor_band_fill(
            bg,
            cursorline,
            CURSOR_BAND_INACTIVE_START,
            CURSOR_BAND_INACTIVE_CONTRAST,
        );
        let mode_input = g.fg_of(&["String"]).unwrap_or(fg);
        let focus_ring = ensure_contrast_with_pole(
            lualine_block(&g, "lualine_a_insert").map_or(mode_input, |(fill, _)| fill),
            bg,
            fg,
            UI_CONTRAST,
        );

        let focus_ring_inactive = cursor_band_fill(bg, focus_ring, 0.12, FOCUS_RING_INACTIVE_CONTRAST);

        let syntax: Vec<(&'static str, Rgb)> = SYNTAX
            .iter()
            .map(|&(name, ts, legacy)| (name, g.fg_of(&[ts, legacy]).unwrap_or(fg)))
            .collect();
        let syn_function = syntax
            .iter()
            .find(|(name, _)| *name == "function")
            .map_or(fg, |(_, colour)| *colour);
        let link = g.fg_of(&LINK_GROUPS).unwrap_or(syn_function);

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
            chrome_accent: ensure_contrast_against_both(
                function.unwrap_or(chrome_fg),
                chrome,
                bg,
                chrome_fg,
                UI_CONTRAST,
            ),
            border: g
                .fg_of(&["WinSeparator", "VertSplit"])
                .unwrap_or_else(|| bg.mix(fg, 0.15)),
            muted,
            cursorline,
            hint_bg,
            hint_fg,
            search: g.bg_of(&["Search"]).unwrap_or(hint_bg),
            warn,
            error,
            ok: signal(&["DiagnosticOk", "DiagnosticInfo"], FALLBACK_OK),
            tint,
            tint_fg,
            tint_muted,
            warn_tint,
            warn_tint_fg,
            warn_tint_muted,
            error_tint,
            error_tint_fg,
            error_tint_muted,
            status_normal: status_block(&g, "lualine_a_normal", cursorline, fg),
            status_insert: status_block(&g, "lualine_a_insert", cursorline, fg),
            status_visual: status_block(&g, "lualine_a_visual", cursorline, fg),
            mode_browse: function.unwrap_or(fg),
            mode_input,
            cursor_band,
            cursor_band_inactive,
            focus_ring,
            focus_ring_inactive,
            link,
            syntax,
            font_mono: mono_font_stack(&payload.options.guifont),
            font_size_px: DEFAULT_PANEL_FONT_SIZE_PX,
            color_scheme: if light { "light" } else { "dark" },
            editor_row_px: None,
            editor_cmd_rows: payload.options.cmdheight,
        }
    }

    /// Colours, font stacks and colour scheme a page beside the editor takes to look like it.
    pub fn appearance_vars(&self) -> Vec<(String, String)> {
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
            ("tint", self.tint),
            ("tint-fg", self.tint_fg),
            ("tint-muted", self.tint_muted),
            ("warn-tint", self.warn_tint),
            ("warn-tint-fg", self.warn_tint_fg),
            ("warn-tint-muted", self.warn_tint_muted),
            ("error-tint", self.error_tint),
            ("error-tint-fg", self.error_tint_fg),
            ("error-tint-muted", self.error_tint_muted),
            ("mode-browse", self.mode_browse),
            ("mode-input", self.mode_input),
            ("status-normal-bg", self.status_normal.0),
            ("status-normal-fg", self.status_normal.1),
            ("status-insert-bg", self.status_insert.0),
            ("status-insert-fg", self.status_insert.1),
            ("status-visual-bg", self.status_visual.0),
            ("status-visual-fg", self.status_visual.1),
            ("cursor-band", self.cursor_band),
            ("cursor-band-inactive", self.cursor_band_inactive),
            ("focus-ring", self.focus_ring),
            ("focus-ring-inactive", self.focus_ring_inactive),
            ("link", self.link),
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
        vars.push(("--nv-color-scheme".to_string(), self.color_scheme.to_string()));
        vars
    }

    /// Sizes measured from this window, which a page with its own zoom must not take.
    pub fn metric_vars(&self) -> Vec<(String, String)> {
        // `{}` on an `f32` prints `14`, not `14.0`, so the common case reads as it was typed.
        let mut vars = vec![("--nv-font-size".to_string(), format!("{}px", self.font_size_px))];
        if let Some(px) = self.editor_row_px {
            vars.push(("--nv-editor-row".to_string(), format!("{px}px")));
            if let Some(rows) = self.editor_cmd_rows {
                vars.push(("--nv-editor-cmd-rows".to_string(), rows.to_string()));
            }
        }
        vars
    }

    pub fn css_vars(&self) -> Vec<(String, String)> {
        let mut vars = self.appearance_vars();
        vars.extend(self.metric_vars());
        vars
    }
}

/// Plain `guifont` families for a font manager, in preference order, each read as Neovide reads it
/// (`font_options.rs`, `parse_font_name`): trimmed, then a backslash takes the next character as it
/// is and `_` is a space, so `\_` is an underscore and `\L` an `L`. Options begin at the first `:`
/// (a value that starts with `:` names no family; the editor then draws its default face). These
/// names never enter CSS, so they keep all other characters and have no CSS family or list limits.
pub fn guifont_families(guifont: &str) -> Vec<String> {
    guifont
        .split(':')
        .next()
        .unwrap_or("")
        .split(',')
        .map(str::trim_ascii)
        .filter(|family| !family.is_empty())
        .map(unescape_font_name)
        .filter(|family| !family.is_empty())
        .collect()
}

/// One `guifont` family as Neovide reads it: `\x` is `x`, `_` is a space, anything else itself.
fn unescape_font_name(family: &str) -> String {
    let mut out = String::with_capacity(family.len());
    let mut chars = family.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.extend(chars.next()),
            '_' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// `guifont` as a CSS `font-family` list, parsed the way Neovide does (`font_options.rs`): options
/// start at the first `:`, families are comma-separated, and `_` or `\ ` stand for a space.
///
/// Characters that could end a CSS value or quoted family, or open a CSS group or comment
/// (`" \ ; { } < > ( ) [ ] / *`, C0, DEL and C1), are dropped. Family and list limits keep the
/// list inside the page font rule by construction. The value reaches the WebView through
/// `setProperty`, so this is defence in depth against whatever sets `guifont` inside nvim.
pub(crate) fn mono_font_stack(guifont: &str) -> String {
    let families = guifont.split(':').next().unwrap_or("");
    let mut stack = Vec::new();
    let mut user_chars = 0;
    let fallback_chars = MONO_FONT_FALLBACK.chars().count();
    for family in families.split(',') {
        let family = family.replace("\\ ", " ").replace('_', " ");
        let family: String = family
            .chars()
            .filter(|&c| {
                !matches!(
                    c,
                    '"' | '\\' | ';' | '{' | '}' | '<' | '>' | '(' | ')' | '[' | ']' | '/' | '*'
                ) && c >= ' '
                    && !('\u{7f}'..='\u{9f}').contains(&c)
            })
            .collect();
        let family: String = family.trim().chars().take(FONT_FAMILY_MAX_CHARS).collect();
        let family = family.trim();
        if family.is_empty() {
            continue;
        }
        if stack.len() >= FONT_USER_MAX_FAMILIES {
            break;
        }
        let quoted = format!("\"{family}\"");
        let next_chars = user_chars + if stack.is_empty() { 0 } else { 2 } + quoted.chars().count();
        if next_chars + 2 + fallback_chars > FONT_LIST_MAX_CHARS {
            break;
        }
        user_chars = next_chars;
        stack.push(quoted);
    }
    stack.push(MONO_FONT_FALLBACK.to_string());
    stack.join(", ")
}

/// A statusline mode section's `(bg, fg)` as the group draws it, or `fallback` when the group is missing
/// or leaves either colour to Normal (`ThemeTokens::status_normal`).
fn status_block(g: &Groups<'_>, group: &str, fallback_bg: Rgb, fallback_fg: Rgb) -> (Rgb, Rgb) {
    lualine_block(g, group).unwrap_or((fallback_bg, fallback_fg))
}

/// A lualine mode section's `(bg, fg)`, or `None` when the group is missing or leaves either colour to Normal.
fn lualine_block(g: &Groups<'_>, group: &str) -> Option<(Rgb, Rgb)> {
    match g.payload.groups.get(group).map(|attrs| g.effective(attrs)) {
        Some((Some(fg), Some(bg))) => Some((bg, fg)),
        _ => None,
    }
}

/// `bg` mixed toward `cursorline` in 2% steps from `start`, stopping at the first step that reaches `min`
/// contrast against `bg`; `cursorline` itself when none does (the fill never goes past it).
fn cursor_band_fill(bg: Rgb, cursorline: Rgb, start: f64, min: f64) -> Rgb {
    let mut amount = start;
    while amount < 1.0 {
        let candidate = bg.mix(cursorline, amount);
        if candidate.contrast(bg) >= min {
            return candidate;
        }
        amount += 0.02;
    }
    cursorline
}

/// Neovide's `guifont` height when the option names none, in points (`font_options.rs`,
/// `DEFAULT_FONT_SIZE`).
const DEFAULT_GUIFONT_POINTS: f32 = 14.0;

/// The size the editor draws its text at, in px before `g:neovide_scale_factor`, read from `guifont` the
/// way Neovide reads it (`font_options.rs`, `FontOptions::parse`): the options follow the first non-empty
/// `:`-separated part, the last `h<points>` among them wins, and points become px by Neovide's own
/// `points_to_pixels` -- 96/72 everywhere but macOS, where a point is a pixel. A height Neovide would
/// refuse (not a number, or not a positive finite one) reads as the default, 14 points.
///
/// The agent panel's text follows it by default, so the two panes beside each other are one size.
pub fn guifont_size_px(guifont: &str) -> f32 {
    let points = guifont
        .split(':')
        .filter(|part| !part.is_empty())
        .skip(1)
        .filter_map(|part| part.strip_prefix('h').filter(|rest| !rest.is_empty()))
        .last()
        .map(|rest| rest.parse::<f32>().ok().filter(|p| p.is_finite() && *p > 0.0))
        .unwrap_or(Some(DEFAULT_GUIFONT_POINTS))
        .unwrap_or(DEFAULT_GUIFONT_POINTS);
    if cfg!(target_os = "macos") {
        points
    } else {
        points * 96.0 / 72.0
    }
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
                cmdheight: None,
                start_scale: None,
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
        // 45, the status band's three mode blocks as a bg and fg each (2026-10-15), the cursor band's two
        // fills with the composer's focus ring, its inactive mark, and the colour of a Markdown link.
        assert_eq!(vars.len(), 56);
        assert!(vars
            .iter()
            .all(|(name, value)| name.starts_with("--nv-") && !value.is_empty()));
        let names: std::collections::HashSet<_> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.len(), 56, "no duplicate variable names");
        // The count alone would be satisfied by any variable at all, and this one carries a UNIT --
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

    /// Without a reported cell height the panel keeps `index.css`'s 24px fallback. The shell sets
    /// it as soon as `neovide-editor`'s `connect_cell_size_changed` reports one.
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

    /// The command line's rows under the editor's statusline lift the panel's band by as many rows, and
    /// only where there is an editor row to line up with.
    #[test]
    fn the_command_line_height_is_a_css_var_only_beside_a_measured_editor() {
        let mut p = payload("dark", &[]);
        p.options.cmdheight = Some(1);
        let mut t = ThemeTokens::derive(&p);
        assert!(
            t.css_vars().iter().all(|(n, _)| n != "--nv-editor-cmd-rows"),
            "no editor row yet: nothing to line up with"
        );
        t.editor_row_px = Some(22.0);
        assert!(t
            .css_vars()
            .contains(&("--nv-editor-cmd-rows".to_string(), "1".to_string())));
        // A snippet older than the field: unknown, so no lift at all rather than a guessed one.
        let mut old = ThemeTokens::derive(&payload("dark", &[]));
        old.editor_row_px = Some(22.0);
        assert!(old.css_vars().iter().all(|(n, _)| n != "--nv-editor-cmd-rows"));
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

    /// A Markdown link in a plugin page takes the colour the editor gives a Markdown link: the Treesitter label
    /// group, then the link group, then the legacy `Underlined`, then whatever `--nv-syn-function` is.
    #[test]
    fn the_link_colour_follows_nvims_markdown_link_highlight() {
        let normal = ("Normal", hl(Some(0xe0e2ea), Some(0x14161b)));
        let function = ("Function", hl(Some(0x111111), None));
        let label = ("@markup.link.label", hl(Some(0x222222), None));
        let link = ("@markup.link", hl(Some(0x333333), None));
        let underlined = ("Underlined", hl(Some(0x444444), None));

        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                normal.clone(),
                function.clone(),
                label.clone(),
                link.clone(),
                underlined.clone(),
            ],
        ));
        assert_eq!(t.link.hex(), "#222222");
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[normal.clone(), function.clone(), link.clone(), underlined.clone()],
        ));
        assert_eq!(t.link.hex(), "#333333");
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[normal.clone(), function.clone(), underlined.clone()],
        ));
        assert_eq!(t.link.hex(), "#444444");

        // No link group at all: the value --nv-syn-function has, whichever group gave it.
        let t = ThemeTokens::derive(&payload("dark", &[normal.clone(), function.clone()]));
        assert_eq!(t.link.hex(), "#111111");
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                normal.clone(),
                function.clone(),
                ("@function", hl(Some(0x555555), None)),
            ],
        ));
        assert_eq!(t.link.hex(), "#555555");
        let t = ThemeTokens::derive(&payload("dark", &[normal.clone()]));
        assert_eq!(t.link, t.fg);

        // A link group that sets no foreground (nvim's own `Underlined` is only an underline) falls through.
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[normal.clone(), function.clone(), ("Underlined", hl(None, None))],
        ));
        assert_eq!(t.link.hex(), "#111111");

        let vars = t.appearance_vars();
        let syn_function = vars
            .iter()
            .find(|(n, _)| n == "--nv-syn-function")
            .map(|(_, v)| v.clone());
        assert_eq!(
            vars.iter().find(|(n, _)| n == "--nv-link").map(|(_, v)| v.clone()),
            syn_function
        );
    }

    /// The shared fixtures carry the new name, with the value their palettes give it: neither palette sets a
    /// link group, so each equals that fixture's own --nv-syn-function.
    #[test]
    fn every_theme_fixture_carries_the_link_colour() {
        let ev: serde_json::Value = serde_json::from_str(include_str!("../plugin/fixtures/ev-theme.json")).unwrap();
        let hello: serde_json::Value = serde_json::from_str(include_str!("../plugin/fixtures/rep-hello.json")).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(include_str!("../plugin/fixtures/rep-hello-settings.json")).unwrap();
        for (name, vars) in [
            ("ev-theme.json", &ev["data"]["vars"]),
            ("rep-hello.json", &hello["result"]["theme"]["vars"]),
            ("rep-hello-settings.json", &settings["result"]["theme"]["vars"]),
        ] {
            assert!(vars["--nv-link"].is_string(), "{name}: no --nv-link");
            assert_eq!(vars["--nv-link"], vars["--nv-syn-function"], "{name}");
        }
        assert_eq!(hello["result"]["theme"]["vars"]["--nv-link"], "#e0e2ea");
        assert_eq!(ev["data"]["vars"]["--nv-link"], "#575279");
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
            assert!(
                t.chrome_accent.contrast(t.bg) >= 3.0,
                "{background}: chrome_accent on bg, {:.2}",
                t.chrome_accent.contrast(t.bg)
            );
        }
    }

    /// Schemes chosen to be hard on the tints and on `chrome_accent`: nvim's own default both ways,
    /// the shipped ones that stress them (zaibatsu's inverted float, pablo's grey
    /// Visual, lunaperche's and quiet's reversed StatusLine), rose-pine dawn's pale Comment and
    /// mid-luminance accents, a mid-grey background with its Comment across it, a dim low-contrast
    /// scheme whose own `fg` misses 4.5:1, and the preview's four palettes.
    fn hard_palettes() -> Vec<(&'static str, ThemeTokens)> {
        let reversed = HlAttrs {
            fg: None,
            bg: None,
            reverse: true,
        };
        let palettes: Vec<(&'static str, NvimThemePayload)> = vec![
            ("default-dark", payload("dark", &[])),
            ("default-light", payload("light", &[])),
            (
                "rose-pine-dawn",
                payload(
                    "light",
                    &[
                        ("Normal", hl(Some(0x575279), Some(0xfaf4ed))),
                        ("NormalFloat", hl(Some(0x575279), Some(0xfffaf3))),
                        ("StatusLine", hl(Some(0xfaf4ed), Some(0xd7827e))),
                        ("Visual", hl(None, Some(0xdfdad9))),
                        ("Comment", hl(Some(0x9893a5), None)),
                        ("Function", hl(Some(0xd7827e), None)),
                        ("DiagnosticWarn", hl(Some(0xea9d34), None)),
                        ("DiagnosticError", hl(Some(0xb4637a), None)),
                    ],
                ),
            ),
            (
                "zaibatsu",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xffffff), Some(0x0e0024))),
                        ("NormalFloat", hl(Some(0x0e0024), Some(0xffffff))),
                        ("Pmenu", hl(Some(0x0e0024), Some(0xffffff))),
                    ],
                ),
            ),
            (
                "pablo",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xffffff), Some(0x000000))),
                        ("Comment", hl(Some(0x808080), None)),
                        ("Visual", hl(None, Some(0xa9a9a9))),
                    ],
                ),
            ),
            (
                "lunaperche",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xc6c6c6), Some(0x000000))),
                        ("StatusLine", reversed.clone()),
                        ("Comment", hl(Some(0x949494), None)),
                    ],
                ),
            ),
            (
                "quiet",
                payload(
                    "light",
                    &[
                        ("Normal", hl(Some(0x000000), Some(0xd7d7d7))),
                        ("StatusLine", reversed.clone()),
                        ("Comment", hl(Some(0x000000), None)),
                    ],
                ),
            ),
            (
                "mid-grey",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xffffff), Some(0x767676))),
                        ("Comment", hl(Some(0x000000), None)),
                        ("Visual", hl(None, Some(0x666666))),
                        ("DiagnosticError", hl(Some(0x000000), None)),
                        ("Function", hl(Some(0x7a7a7a), None)),
                    ],
                ),
            ),
            (
                "dim-low-contrast",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0x8a8a8a), Some(0x3a3a3a))),
                        ("Comment", hl(Some(0x5a5a5a), None)),
                        ("Function", hl(Some(0x4a4a6a), None)),
                    ],
                ),
            ),
            (
                "tokyonight-night",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xc0caf5), Some(0x1a1b26))),
                        ("NormalFloat", hl(Some(0xc0caf5), Some(0x16161e))),
                        ("StatusLine", hl(Some(0xa9b1d6), Some(0x16161e))),
                        ("Visual", hl(None, Some(0x283457))),
                        ("Comment", hl(Some(0x565f89), None)),
                        ("Function", hl(Some(0x7aa2f7), None)),
                        ("DiagnosticWarn", hl(Some(0xe0af68), None)),
                        ("DiagnosticError", hl(Some(0xf7768e), None)),
                    ],
                ),
            ),
            (
                "gruvbox-dark",
                payload(
                    "dark",
                    &[
                        ("Normal", hl(Some(0xebdbb2), Some(0x282828))),
                        ("NormalFloat", hl(Some(0xebdbb2), Some(0x32302f))),
                        ("StatusLine", hl(Some(0xebdbb2), Some(0x504945))),
                        ("Visual", hl(None, Some(0x504945))),
                        ("Comment", hl(Some(0x928374), None)),
                        ("Function", hl(Some(0xb8bb26), None)),
                        ("DiagnosticWarn", hl(Some(0xfabd2f), None)),
                        ("DiagnosticError", hl(Some(0xfb4934), None)),
                    ],
                ),
            ),
            (
                "catppuccin-latte",
                payload(
                    "light",
                    &[
                        ("Normal", hl(Some(0x4c4f69), Some(0xeff1f5))),
                        ("NormalFloat", hl(Some(0x4c4f69), Some(0xe6e9ef))),
                        ("StatusLine", hl(Some(0x4c4f69), Some(0xe6e9ef))),
                        ("Visual", hl(None, Some(0xccd0da))),
                        ("Comment", hl(Some(0x9ca0b0), None)),
                        ("Function", hl(Some(0x1e66f5), None)),
                        ("DiagnosticWarn", hl(Some(0xdf8e1d), None)),
                        ("DiagnosticError", hl(Some(0xd20f39), None)),
                    ],
                ),
            ),
        ];
        palettes
            .into_iter()
            .map(|(name, p)| (name, ThemeTokens::derive(&p)))
            .collect()
    }

    /// The status band's `auto` and `bypass` glyphs are drawn in `warn` and `error` on `chrome`, so both
    /// reach the non-text threshold against `chrome` as well as `bg`, on every palette, a reversed
    /// StatusLine (where `chrome` is Normal's fg, the far side of mid-grey from `bg`) included.
    #[test]
    fn warn_and_error_are_readable_on_chrome_as_well_as_bg() {
        let mut cases = hard_palettes();
        for background in ["dark", "light"] {
            let (fg, bg) = if background == "dark" {
                (0xe0e0e0, 0x1a1a1a)
            } else {
                (0x202020, 0xf4f4f4)
            };
            let reversed = HlAttrs {
                fg: None,
                bg: None,
                reverse: true,
            };
            cases.push((
                if background == "dark" {
                    "reversed-dark"
                } else {
                    "reversed-light"
                },
                ThemeTokens::derive(&payload(
                    background,
                    &[
                        ("Normal", hl(Some(fg), Some(bg))),
                        ("StatusLine", reversed),
                        ("DiagnosticWarn", hl(Some(0xe5c07b), None)),
                        ("DiagnosticError", hl(Some(0xe06c75), None)),
                    ],
                )),
            ));
        }
        for (name, t) in cases {
            for (colour_name, colour) in [("warn", t.warn), ("error", t.error)] {
                for (surface_name, surface) in [("bg", t.bg), ("chrome", t.chrome)] {
                    assert!(
                        colour.contrast(surface) >= UI_CONTRAST,
                        "{name}: {colour_name} {} on {surface_name} {} is {:.2}:1",
                        colour.hex(),
                        surface.hex(),
                        colour.contrast(surface)
                    );
                }
            }
        }
    }

    /// The text drawn on your message and the composer, on a permission card, and on a failure's tint
    /// reads on the tint under it, whatever the scheme -- the mid-grey one included, where the `fg`
    /// tint and the `error` tint sit on opposite sides of mid-grey.
    #[test]
    fn text_on_the_tints_reaches_text_contrast_on_every_hard_palette() {
        for (name, t) in hard_palettes() {
            assert_eq!(t.tint, t.bg.mix(t.fg, 0.07), "{name}: tint");
            assert_eq!(t.warn_tint, t.bg.mix(t.warn, 0.06), "{name}: warn_tint");
            assert_eq!(t.error_tint, t.bg.mix(t.error, 0.09), "{name}: error_tint");
            for (fill_name, fill, pair) in [
                ("tint", t.tint, [t.tint_fg, t.tint_muted]),
                ("warn_tint", t.warn_tint, [t.warn_tint_fg, t.warn_tint_muted]),
                ("error_tint", t.error_tint, [t.error_tint_fg, t.error_tint_muted]),
            ] {
                for (text_name, text) in ["fg", "muted"].into_iter().zip(pair) {
                    assert!(
                        text.contrast(fill) >= TEXT_CONTRAST,
                        "{name}: {fill_name}_{text_name} {} on {fill_name} {} is {:.2}:1",
                        text.hex(),
                        fill.hex(),
                        text.contrast(fill)
                    );
                }
            }
        }
    }

    /// Where a scheme's own colours already read on the tints, they are what the tints carry: the
    /// guard moves `fg` not at all and `muted` only as far as it must.
    #[test]
    fn the_tint_text_keeps_the_schemes_own_colours_where_they_read() {
        let t = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xc0caf5), Some(0x1a1b26))),
                ("Comment", hl(Some(0x9aa5ce), None)),
            ],
        ));
        for (fg, muted) in [
            (t.tint_fg, t.tint_muted),
            (t.warn_tint_fg, t.warn_tint_muted),
            (t.error_tint_fg, t.error_tint_muted),
        ] {
            assert_eq!(fg, t.fg);
            assert_eq!(
                muted, t.muted,
                "a Comment that already reads on every tint stays itself"
            );
        }
    }

    /// `chrome_accent` is drawn on `chrome` (the status bar's mode block) and on `bg` (the panel's
    /// running and activity glyphs, the chooser's unread dot): 3:1 on both, on every hard palette.
    #[test]
    fn chrome_accent_reaches_ui_contrast_on_chrome_and_on_bg() {
        for (name, t) in hard_palettes() {
            for (fill_name, fill) in [("chrome", t.chrome), ("bg", t.bg)] {
                assert!(
                    t.chrome_accent.contrast(fill) >= UI_CONTRAST,
                    "{name}: chrome_accent {} on {fill_name} {} is {:.2}:1",
                    t.chrome_accent.hex(),
                    fill.hex(),
                    t.chrome_accent.contrast(fill)
                );
            }
        }
    }

    /// The conversation's cursor band is its only cursor mark, so on every hard palette its fill is a mix of
    /// `bg` and `cursorline` that reaches the minimum against `bg` (the weaker one too), or is `cursorline`
    /// itself where even that is as visible as the palette allows; the focused fill is never fainter than
    /// the unfocused one.
    #[test]
    fn the_cursor_band_is_visible_against_bg_on_every_hard_palette() {
        for (name, t) in hard_palettes() {
            for (label, fill, min) in [
                ("cursor_band", t.cursor_band, CURSOR_BAND_CONTRAST),
                (
                    "cursor_band_inactive",
                    t.cursor_band_inactive,
                    CURSOR_BAND_INACTIVE_CONTRAST,
                ),
            ] {
                let contrast = fill.contrast(t.bg);
                assert!(
                    contrast >= min || fill == t.cursorline,
                    "{name}: {label} {} on bg {} is {contrast:.3}:1 (needs {min})",
                    fill.hex(),
                    t.bg.hex()
                );
                assert!(
                    t.cursorline.contrast(t.bg) >= contrast - 1e-9,
                    "{name}: {label} must not go past cursorline"
                );
            }
            assert!(
                t.cursor_band.contrast(t.bg) >= t.cursor_band_inactive.contrast(t.bg),
                "{name}: the focused band is the stronger one"
            );
            assert!(
                t.cursor_band.contrast(t.fg) >= TEXT_CONTRAST || t.fg.contrast(t.bg) < TEXT_CONTRAST,
                "{name}: fg still reads on the band, {:.2}",
                t.cursor_band.contrast(t.fg)
            );
        }
    }

    /// The minimum is a floor the fill is raised to, not a ceiling: the plain 24%/12% mix is kept where it
    /// already shows, and raised where it does not.
    #[test]
    fn the_cursor_band_starts_light_and_is_raised_only_as_far_as_the_minimum_needs() {
        let (bg, line) = (Rgb::from_u32(0x1a1b26), Rgb::from_u32(0x283457));
        // tokyonight-night: 24% / 12% measured 1.06 / 1.03.
        assert!(bg.mix(line, CURSOR_BAND_START).contrast(bg) < CURSOR_BAND_CONTRAST);
        let band = cursor_band_fill(bg, line, CURSOR_BAND_START, CURSOR_BAND_CONTRAST);
        assert!(band.contrast(bg) >= CURSOR_BAND_CONTRAST);
        // One step less would not have reached it: no more is added than needed.
        let steps = ((0..60)
            .map(|i| CURSOR_BAND_START + 0.02 * f64::from(i))
            .find(|a| bg.mix(line, *a) == band)
            .expect("the result is one of the steps")
            - CURSOR_BAND_START)
            / 0.02;
        assert!(steps > 0.5, "raised at least one step");
        let previous = bg.mix(line, CURSOR_BAND_START + 0.02 * (steps.round() - 1.0));
        assert!(previous.contrast(bg) < CURSOR_BAND_CONTRAST);
        // A fill that already shows is left as the light mix.
        let bright = Rgb::from_u32(0x606060);
        assert_eq!(
            cursor_band_fill(bg, bright, CURSOR_BAND_START, CURSOR_BAND_CONTRAST),
            bg.mix(bright, CURSOR_BAND_START)
        );
        // An unreachable minimum stops at cursorline, never past it.
        assert_eq!(
            cursor_band_fill(bg, Rgb::from_u32(0x1b1c27), 0.24, 1.5),
            Rgb::from_u32(0x1b1c27)
        );
        // Reaches the panel as two variables.
        let vars: HashMap<String, String> = ThemeTokens::fallback().css_vars().into_iter().collect();
        assert!(vars["--nv-cursor-band"].starts_with('#') && vars["--nv-cursor-band-inactive"].starts_with('#'));
    }

    #[test]
    fn an_inactive_mark_is_dimmer_than_the_active_one() {
        let mut cases = hard_palettes();
        cases.push((
            "faint-lualine",
            ThemeTokens::derive(&payload(
                "dark",
                &[
                    ("Normal", hl(Some(0xc0caf5), Some(0x1a1b26))),
                    ("lualine_a_insert", hl(Some(0x15161e), Some(0x1e2030))),
                ],
            )),
        ));
        // Eight-bit channels put this ring within a millionth of the contrast floor.
        let boundary = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xffffff), Some(0x000000))),
                ("String", hl(Some(0x960fb1), None)),
            ],
        ));
        assert!((boundary.focus_ring.contrast(boundary.bg) - UI_CONTRAST).abs() < 0.000001);
        cases.push(("ui-contrast-boundary", boundary));
        for (name, t) in cases {
            let inactive = t.focus_ring_inactive.contrast(t.bg);
            let active = t.focus_ring.contrast(t.bg);
            assert!(
                inactive >= 1.6 && inactive < active,
                "{name}: inactive {inactive:.6}, active {active:.6}"
            );
            for vars in [t.css_vars(), t.appearance_vars()] {
                assert_eq!(
                    vars.iter()
                        .find(|(n, _)| n == "--nv-focus-ring-inactive")
                        .map(|(_, v)| v),
                    Some(&t.focus_ring_inactive.hex()),
                    "{name}"
                );
            }
        }
    }

    /// The composer's focus ring is a border and outline, so it is held to the non-text 3:1 against `bg` on
    /// every hard palette, with lualine's insert section or without it. A lualine insert block that is
    /// itself the faint `cursorline` stand-in must not be what the ring falls back to.
    #[test]
    fn the_composer_focus_ring_reaches_ui_contrast_on_bg_with_and_without_lualine() {
        for (name, t) in hard_palettes() {
            assert!(
                t.focus_ring.contrast(t.bg) >= UI_CONTRAST,
                "{name}: focus_ring {} on bg {} is {:.2}:1",
                t.focus_ring.hex(),
                t.bg.hex(),
                t.focus_ring.contrast(t.bg)
            );
        }
        let normal = ("Normal", hl(Some(0xc0caf5), Some(0x1a1b26)));
        // No lualine at all: not the 1.15:1 cursorline stand-in `status_insert` falls back to.
        let plain = ThemeTokens::derive(&payload(
            "dark",
            &[normal.clone(), ("String", hl(Some(0x9ece6a), None))],
        ));
        assert_eq!(plain.status_insert.0, plain.cursorline);
        assert!(
            plain.status_insert.0.contrast(plain.bg) < UI_CONTRAST,
            "the stand-in really is faint"
        );
        assert!(plain.focus_ring.contrast(plain.bg) >= UI_CONTRAST);
        assert_eq!(
            plain.focus_ring,
            Rgb::from_u32(0x9ece6a),
            "String's colour already reads: kept as it is"
        );
        // No lualine and no String either: fg-ward from the fallback, still 3:1.
        let bare = ThemeTokens::derive(&payload("dark", &[normal.clone()]));
        assert!(bare.focus_ring.contrast(bare.bg) >= UI_CONTRAST);
        // A lualine insert block too close to bg is walked toward fg until it reads; one that reads is kept.
        let faint = ThemeTokens::derive(&payload(
            "dark",
            &[normal.clone(), ("lualine_a_insert", hl(Some(0x15161e), Some(0x1e2030)))],
        ));
        assert_eq!(faint.status_insert.0, Rgb::from_u32(0x1e2030));
        assert!(faint.focus_ring.contrast(faint.bg) >= UI_CONTRAST);
        let strong = ThemeTokens::derive(&payload(
            "dark",
            &[normal, ("lualine_a_insert", hl(Some(0x15161e), Some(0x9ece6a)))],
        ));
        assert_eq!(strong.focus_ring, Rgb::from_u32(0x9ece6a));
        let vars: HashMap<String, String> = strong.css_vars().into_iter().collect();
        assert_eq!(vars["--nv-focus-ring"], "#9ece6a");
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

    /// Neovide's `points_to_pixels` off macOS.
    fn px(points: f32) -> f32 {
        if cfg!(target_os = "macos") {
            points
        } else {
            points * 96.0 / 72.0
        }
    }

    /// Rose-pine dawn with LazyVim's lualine: the mode block
    /// is lualine's own pair, unguarded, and a scheme with no lualine keeps the guarded cursorline block.
    #[test]
    fn the_status_band_mode_block_copies_lualines_mode_sections() {
        let t = ThemeTokens::derive(&payload(
            "light",
            &[
                ("Normal", hl(Some(0x575279), Some(0xfaf4ed))),
                ("lualine_a_normal", hl(Some(0xfaf4ed), Some(0xd7827e))),
                ("lualine_a_insert", hl(Some(0xfaf4ed), Some(0x56949f))),
                ("lualine_a_visual", hl(Some(0xfaf4ed), Some(0x907aa9))),
            ],
        ));
        assert_eq!(t.status_normal, (Rgb::from_u32(0xd7827e), Rgb::from_u32(0xfaf4ed)));
        assert_eq!(t.status_insert, (Rgb::from_u32(0x56949f), Rgb::from_u32(0xfaf4ed)));
        assert_eq!(t.status_visual, (Rgb::from_u32(0x907aa9), Rgb::from_u32(0xfaf4ed)));
        // Not guarded: the editor draws this very pair, below 4.5:1.
        assert!(t.status_normal.0.contrast(t.status_normal.1) < 4.5);
        let vars: HashMap<String, String> = t.css_vars().into_iter().collect();
        assert_eq!(vars["--nv-status-normal-bg"], "#d7827e");
        assert_eq!(vars["--nv-status-insert-fg"], "#faf4ed");
        assert_eq!(vars["--nv-status-visual-bg"], "#907aa9");

        // No lualine, or a section that leaves a colour to Normal: the guarded cursorline block.
        let plain = ThemeTokens::derive(&payload(
            "dark",
            &[
                ("Normal", hl(Some(0xc0caf5), Some(0x1a1b26))),
                ("lualine_a_insert", hl(Some(0x15161e), None)),
            ],
        ));
        for block in [plain.status_normal, plain.status_insert, plain.status_visual] {
            assert_eq!(block, (plain.cursorline, plain.fg));
            assert!(block.0.contrast(block.1) >= 4.5);
        }
    }

    #[test]
    fn guifont_families_are_plain_names_for_the_font_manager() {
        for (value, expected) in [
            ("", vec![]),
            (
                "Fira\\ Code,Symbols_Nerd_Font:h12:b",
                vec!["Fira Code", "Symbols Nerd Font"],
            ),
            (":h12", vec![]),
            ("a,,b", vec!["a", "b"]),
            ("  a  , , b_ :h12", vec!["a", "b "]),
            ("a(b)\";{}", vec!["a(b)\";{}"]),
            // A backslash takes the next character as it is, as in the editor.
            ("\\Liberation_Mono:h12", vec!["Liberation Mono"]),
            ("Under\\_Score,Back\\\\slash", vec!["Under_Score", "Back\\slash"]),
            ("trailing\\", vec!["trailing"]),
            ("\\", vec![]),
        ] {
            assert_eq!(guifont_families(value), expected, "{value:?}");
        }
    }

    #[test]
    fn the_editor_font_size_is_guifonts_height_converted_the_way_neovide_converts_it() {
        // Unset, or no height: Neovide's own 14 points.
        assert_eq!(guifont_size_px(""), px(14.0));
        assert_eq!(guifont_size_px("Source Code Pro,DejaVu Sans Mono,monospace"), px(14.0));
        assert_eq!(guifont_size_px("FiraCode Nerd Font:h12"), px(12.0));
        assert_eq!(guifont_size_px("Iosevka:b:h13.5:i"), px(13.5));
        // The last height wins, as Neovide's loop over the options leaves it.
        assert_eq!(guifont_size_px("Iosevka:h10:h16"), px(16.0));
        // The first non-empty part is the family list, even when it looks like a height.
        assert_eq!(guifont_size_px(":h12"), px(14.0));
        assert_eq!(guifont_size_px("Mono::h11"), px(11.0));
        // A hinting option is not a height.
        assert_eq!(guifont_size_px("Mono:#h-full"), px(14.0));
        // What Neovide would refuse reads as the default.
        for bad in ["Mono:hx", "Mono:h-3", "Mono:h0", "Mono:hNaN", "Mono:hinf"] {
            assert_eq!(guifont_size_px(bad), px(14.0), "{bad}");
        }
        // A desktop with no height, so 18.67px on Linux.
        if !cfg!(target_os = "macos") {
            assert!(
                (guifont_size_px("Source Code Pro,DejaVu Sans Mono,Courier New,monospace") - 18.666_666).abs() < 1e-3
            );
        }
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

    /// The shipped `zaibatsu` colorscheme's real highlight data (measured
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

    /// The shipped `pablo` colorscheme's real highlight data (`Comment`
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

    /// Moving `muted` toward `fg` to clear `cursorline` can reduce its contrast against `bg`
    /// when `muted` and `fg` lie on opposite sides of `bg`. A black Comment on mid-grey passes
    /// the first guard at 4.62:1, but mixing toward white can yield `#e6e6e6`: 4.60:1 against
    /// `cursorline`, only 3.64:1 against `bg`. Muted text is painted on both and must read on both.
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

    /// A real payload fed through `ThemeTokens::derive`
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

    const FALLBACK_TAIL: &str = "\"FiraCode Nerd Font\", monospace";

    #[test]
    fn css_vars_are_the_appearance_vars_then_the_metric_vars() {
        let mut t = ThemeTokens::fallback();
        t.editor_row_px = Some(22.0);
        t.editor_cmd_rows = Some(2);
        let appearance = t.appearance_vars();
        let metric = t.metric_vars();
        let mut joined = appearance.clone();
        joined.extend(metric.clone());
        assert_eq!(t.css_vars(), joined);
        let names: std::collections::HashSet<&str> = appearance.iter().map(|(n, _)| n.as_str()).collect();
        assert!(metric.iter().all(|(n, _)| !names.contains(n.as_str())));
    }

    #[test]
    fn the_metric_vars_are_the_sizes_measured_from_the_window() {
        let mut t = ThemeTokens::fallback();
        t.editor_row_px = Some(22.0);
        t.editor_cmd_rows = Some(2);
        let names: Vec<String> = t.metric_vars().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["--nv-font-size", "--nv-editor-row", "--nv-editor-cmd-rows"]);
        assert_eq!(
            ThemeTokens::fallback()
                .metric_vars()
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            ["--nv-font-size"]
        );
    }

    #[test]
    fn the_appearance_vars_number_55_and_carry_no_size() {
        let vars = ThemeTokens::fallback().appearance_vars();
        assert_eq!(vars.len(), 55);
        let names: std::collections::HashSet<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.len(), 55, "no duplicate names");
        for name in [
            "--nv-bg",
            "--nv-focus-ring",
            "--nv-focus-ring-inactive",
            "--nv-syn-tag",
            "--nv-font-prose",
            "--nv-font-mono",
            "--nv-color-scheme",
        ] {
            assert!(names.contains(name), "{name}");
        }
        for name in ["--nv-font-size", "--nv-editor-row", "--nv-editor-cmd-rows"] {
            assert!(!names.contains(name), "{name}");
        }
    }

    #[test]
    fn guifont_loses_every_character_that_could_open_a_css_group_or_comment() {
        assert_eq!(
            mono_font_stack("a(b)c[d]e/f*g\u{7}h\u{7f}i\u{85}j\u{9f}k:h12"),
            "\"abcdefghijk\", \"FiraCode Nerd Font\", monospace"
        );
    }

    #[test]
    fn each_guifont_family_is_cut_to_100_characters() {
        let long = "x".repeat(150);
        assert_eq!(
            mono_font_stack(&long),
            format!("\"{}\", {FALLBACK_TAIL}", "x".repeat(100))
        );
        let wide = "\u{1D538}".repeat(101);
        assert_eq!(
            mono_font_stack(&wide),
            format!("\"{}\", {FALLBACK_TAIL}", "\u{1D538}".repeat(100))
        );
    }

    #[test]
    fn at_most_14_guifont_families_are_kept() {
        let many: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
        let kept: Vec<String> = (0..14).map(|i| format!("\"f{i}\"")).collect();
        assert_eq!(
            mono_font_stack(&many.join(",")),
            format!("{}, {FALLBACK_TAIL}", kept.join(", "))
        );
    }

    #[test]
    fn the_font_list_stops_growing_before_1024_characters() {
        // Nine families of 100 fit (967 characters with the fallback); the tenth would pass 1024, and the
        // short one after it is not tried.
        let mut families: Vec<String> = (0..10u8)
            .map(|i| format!("{}{}", char::from(b'a' + i), "x".repeat(99)))
            .collect();
        families.push("z".into());
        let stack = mono_font_stack(&families.join(","));
        assert!(stack.ends_with(FALLBACK_TAIL));
        assert_eq!(stack.chars().count(), 967);
        assert_eq!(stack.matches('"').count(), 2 * 9 + 2);
        assert!(!stack.contains("\"z\""));
    }

    /// Long and hostile guifont values, from a fixed seed: every output is a list a page's font rule accepts.
    #[test]
    fn every_guifont_gives_a_font_list_inside_the_page_rule() {
        const POOL: &[char] = &[
            'a',
            'Z',
            '0',
            ' ',
            '_',
            '-',
            '.',
            '@',
            '\'',
            '\u{e9}',
            '\u{4e2d}',
            '\u{1D538}',
            '\u{2028}',
            '\u{a0}',
            '\u{5c}',
            '"',
            ';',
            '{',
            '}',
            '<',
            '>',
            '(',
            ')',
            '[',
            ']',
            '/',
            '*',
            '\u{0}',
            '\n',
            '\t',
            '\u{1f}',
            '\u{7f}',
            '\u{80}',
            '\u{85}',
            '\u{9f}',
        ];
        let refused = |c: char| {
            matches!(
                c,
                '"' | '\u{5c}' | ';' | '{' | '}' | '<' | '>' | '(' | ')' | '[' | ']' | '/' | '*'
            ) || c < ' '
                || ('\u{7f}'..='\u{9f}').contains(&c)
        };
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, bound: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % bound as u64) as usize
            }
        }
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            let mut families = Vec::new();
            for _ in 0..1 + rng.below(40) {
                let mut family = String::new();
                for _ in 0..1 + rng.below(150) {
                    family.push(POOL[rng.below(POOL.len())]);
                }
                families.push(family);
            }
            let guifont = families.join(",");
            let stack = mono_font_stack(&guifont);
            assert!(stack.chars().count() <= FONT_LIST_MAX_CHARS, "{guifont:?}");
            let user = stack
                .strip_suffix(FALLBACK_TAIL)
                .unwrap_or_else(|| panic!("no fallback: {guifont:?}"));
            if user.is_empty() {
                continue;
            }
            let user = user.strip_suffix(", ").unwrap_or_else(|| panic!("{stack:?}"));
            let kept: Vec<&str> = user.split(", ").collect();
            assert!(kept.len() <= FONT_USER_MAX_FAMILIES, "{guifont:?}");
            for family in kept {
                let inner = family
                    .strip_prefix('"')
                    .and_then(|f| f.strip_suffix('"'))
                    .unwrap_or_else(|| panic!("unquoted {family:?}"));
                assert!(
                    (1..=FONT_FAMILY_MAX_CHARS).contains(&inner.chars().count()),
                    "{family:?}"
                );
                assert!(!inner.chars().any(refused), "{family:?}");
            }
        }
    }
}
