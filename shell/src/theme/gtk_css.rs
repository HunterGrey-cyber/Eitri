//! The GTK half of the theme: one stylesheet, reloaded in place when nvim's colours change.

use gtk4::CssProvider;

use neovibe_core::theme::tokens::{ThemeTokens, PROSE_FONT_STACK};

/// Overrides GTK/Adwaita on every class the shell draws, so none of the default theme leaks
/// through. Class names are unchanged from the placeholder palette this replaced; restyling the
/// chrome itself is sub-project 2 of the UI spec.
pub(crate) fn gtk_css(tokens: &ThemeTokens) -> String {
    let bg = tokens.bg.hex();
    let fg = tokens.fg.hex();
    let chrome = tokens.chrome.hex();
    let chrome_fg = tokens.chrome_fg.hex();
    // Every text colour drawn on `chrome` comes from a token guarded against `chrome`. The
    // bg-guarded `muted` and the unguarded `mode_browse` are never used on chrome: a reversed
    // StatusLine makes `chrome` the colour they were pushed toward. `muted` does appear below, as
    // the focus outline, and that outline is drawn on `bg`, which is the surface it is guarded against.
    let chrome_muted = tokens.chrome_muted.hex();
    let chrome_accent = tokens.chrome_accent.hex();
    let border = tokens.border.hex();
    // The focused pane's outline. It is drawn on `bg`: the editor paints nvim's `Normal` background,
    // and the panel paints `--nv-bg`. So it needs a colour guarded against `bg`. The accent tokens
    // are not: `chrome_accent` is guarded against `chrome`, and `mode_browse` is not guarded at all.
    // `muted` is guarded at TEXT_CONTRAST (4.5:1) against `bg`. That is more than the 3:1 WCAG
    // 1.4.11 asks of a focus indicator, so this reuses an existing token and derives no new one.
    let focus_ring = tokens.muted.hex();
    let hover = tokens.cursorline.hex();
    let error = tokens.error.hex();
    let font = PROSE_FONT_STACK;

    format!(
        r#"
window {{
    background-color: {bg};
    color: {fg};
    font-family: {font};
}}

.shell-root {{
    background-color: {bg};
}}

.topbar {{
    background-color: {chrome};
    color: {chrome_fg};
    border-bottom: 1px solid {border};
    padding: 0 8px;
    min-height: 38px;
}}

.topbar-app-name {{
    color: {chrome_fg};
    font-weight: 700;
    font-size: 13px;
    margin-right: 8px;
}}

.topbar-project-name {{
    color: {chrome_muted};
    font-size: 12px;
}}

.win-btn {{
    background-color: transparent;
    background-image: none;
    border: none;
    box-shadow: none;
    color: {chrome_muted};
    min-width: 28px;
    min-height: 28px;
    padding: 0;
    border-radius: 6px;
}}

.win-btn:hover {{
    background-color: {hover};
    color: {fg};
}}

.win-btn.close:hover {{
    background-color: {error};
    color: {bg};
}}

.content-area {{
    background-color: {bg};
}}

paned.content-area > separator {{
    background-color: {border};
    background-image: none;
    min-width: 1px;
    min-height: 1px;
}}

.pane-placeholder {{
    background-color: {chrome};
    color: {chrome_muted};
    font-size: 12px;
}}

.pane-placeholder.left {{
    background-color: {bg};
}}

.statusbar {{
    background-color: {chrome};
    border-top: 1px solid {border};
    padding: 0 8px;
    min-height: 24px;
    color: {chrome_muted};
    font-size: 11px;
}}

.statusbar-caption {{
    color: {chrome_muted};
    margin-right: 6px;
}}

/* The focused pane's name. It is text on chrome, so it is `chrome_fg` (guarded at 4.5:1 against
   chrome). The accent goes on the rule beside it, never on the glyphs: `chrome_accent` is guarded
   only at 3:1. */
.statusbar-focus {{
    color: {chrome_fg};
    font-weight: 700;
    border-left: 3px solid {chrome_accent};
    padding-left: 5px;
}}

/* `outline`, never `border`: a border takes space from the widget's content box, so focusing the
   editor would shrink the GL area by 4px and resize nvim's grid on every Ctrl+h / Ctrl+l. An
   outline takes no layout space. GTK draws it after the widget's content, and the negative offset
   keeps it inside the pane's own allocation, where the neighbouring pane cannot paint over it.
   None of that has been seen on a screen. */
.pane.pane-focused {{
    outline: 2px solid {focus_ring};
    outline-offset: -2px;
}}
"#
    )
}

/// One provider for the process's display, reloaded rather than stacked: adding a second provider
/// per colorscheme change would leave every earlier palette underneath, still matching.
#[derive(Clone)]
pub(crate) struct ThemeCss {
    provider: CssProvider,
}

impl ThemeCss {
    pub(crate) fn install(tokens: &ThemeTokens) -> Self {
        let provider = CssProvider::new();
        provider.load_from_string(&gtk_css(tokens));
        let display = gtk4::gdk::Display::default().expect("no default GDK display");
        // `STYLE_PROVIDER_PRIORITY_USER` (800), not `_APPLICATION` (600): GTK loads
        // `$XDG_CONFIG_HOME/gtk-4.0/gtk.css` at `_USER` into the same cascade, and at equal
        // priority the *later*-added provider wins (gtkstylecascade's insertion order), so adding
        // ours here — after GTK's own already-registered user provider — makes it win regardless
        // of selector specificity. This deliberately overrides GTK's "user has the last word"
        // convention, because every colour in this window is meant to come from nvim (spec §1;
        // repo `CLAUDE.md`'s "Division of responsibility": "GTK's default (Adwaita) appearance
        // suppressed"). At `_APPLICATION` a foreign `~/.config/gtk-4.0/gtk.css` (a real one exists
        // on this project's own dev machine) would win over `.win-btn`/the paned divider/etc, which
        // is not "GTK providing windowing only" — it's a second, uncontrolled theme leaking through
        // the one this crate exists to paint. See the Task 6 review finding this responds to and
        // Task 10 Step 4 check 1, which verifies this against a real `gtk.css`.
        gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
        ThemeCss { provider }
    }

    pub(crate) fn update(&self, tokens: &ThemeTokens) {
        self.provider.load_from_string(&gtk_css(tokens));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neovibe_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};

    fn dawn() -> ThemeTokens {
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [("Normal".to_string(), HlAttrs { fg: Some(0x575279), bg: Some(0xfaf4ed), reverse: false })].into(),
            options: NvimOptions { background: "light".into(), guifont: String::new(), colors_name: "rose-pine".into() },
        })
    }

    #[test]
    fn the_window_is_painted_with_nvims_normal() {
        let css = gtk_css(&dawn());
        assert!(css.contains("background-color: #faf4ed;"), "{css}");
        assert!(css.contains("color: #575279;"), "{css}");
        assert!(css.contains(PROSE_FONT_STACK));
    }

    #[test]
    fn no_colour_from_the_old_placeholder_palette_survives() {
        let css = gtk_css(&dawn());
        for old in ["#1a1b1f", "#222329", "#2b2c34", "#383a44", "#7ca9ff", "rgba("] {
            assert!(!css.contains(old), "found {old}");
        }
    }

    #[test]
    fn braces_balance() {
        let css = gtk_css(&ThemeTokens::fallback());
        assert_eq!(css.matches('{').count(), css.matches('}').count());
    }

    /// The body of the rule whose selector is exactly `selector` (`.statusbar`, not `.statusbar-accent`).
    fn rule<'a>(css: &'a str, selector: &str) -> &'a str {
        let start = css.find(&format!("\n{selector} {{")).unwrap_or_else(|| panic!("no rule {selector}"));
        let body = &css[start..];
        &body[..body.find('}').unwrap()]
    }

    #[test]
    fn text_on_chrome_is_paired_with_chrome_not_with_the_window_background() {
        // lunaperche: reversed StatusLine, no Function. With bg-guarded tokens the status bar's
        // "NORMAL" came out at 1.00:1 on chrome.
        let t = ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                ("Normal".to_string(), HlAttrs { fg: Some(0xc6c6c6), bg: Some(0x000000), reverse: false }),
                ("StatusLine".to_string(), HlAttrs { fg: None, bg: None, reverse: true }),
                ("Comment".to_string(), HlAttrs { fg: Some(0x949494), bg: None, reverse: false }),
            ]
            .into(),
            options: NvimOptions { background: "dark".into(), guifont: String::new(), colors_name: "lunaperche".into() },
        });
        let css = gtk_css(&t);
        for (selector, colour, min) in [
            (".topbar-app-name", t.chrome_fg, 4.5),
            (".topbar-project-name", t.chrome_muted, 4.5),
            (".win-btn", t.chrome_muted, 4.5),
            (".statusbar", t.chrome_muted, 4.5),
            (".statusbar-caption", t.chrome_muted, 4.5),
            (".statusbar-focus", t.chrome_fg, 4.5),
        ] {
            let body = rule(&css, selector);
            assert!(body.contains(&format!("\n    color: {};", colour.hex())), "{selector}: {body}");
            assert!(colour.contrast(t.chrome) >= min, "{selector} is unreadable on chrome");
        }
    }

    /// The focused pane's name is text, so it gets a text-guarded colour. `chrome_accent` (3:1)
    /// is allowed only on the rule beside it.
    #[test]
    fn the_focus_label_puts_the_accent_on_a_rule_never_on_its_text() {
        let t = ThemeTokens::fallback();
        let css = gtk_css(&t);
        let body = rule(&css, ".statusbar-focus");
        assert!(body.contains(&format!("\n    border-left: 3px solid {};", t.chrome_accent.hex())), "{body}");
        let text_colours: Vec<&str> = body.lines().filter(|l| l.trim_start().starts_with("color:")).collect();
        let expected = format!("    color: {};", t.chrome_fg.hex());
        assert_eq!(text_colours, vec![expected.as_str()], "{body}");
    }

    /// The outline is drawn on `bg`, so its colour must reach 3:1 against `bg` (WCAG 1.4.11) on
    /// every scheme. This includes the reversed-StatusLine schemes that broke the chrome tokens.
    /// `muted` is guarded at 4.5 against `bg`, and the test checks the colour the rule uses rather
    /// than trusting that.
    #[test]
    fn the_focus_outline_is_visible_against_the_background_it_sits_on() {
        let lunaperche = ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                ("Normal".to_string(), HlAttrs { fg: Some(0xc6c6c6), bg: Some(0x000000), reverse: false }),
                ("StatusLine".to_string(), HlAttrs { fg: None, bg: None, reverse: true }),
                ("Comment".to_string(), HlAttrs { fg: Some(0x949494), bg: None, reverse: false }),
            ]
            .into(),
            options: NvimOptions { background: "dark".into(), guifont: String::new(), colors_name: "lunaperche".into() },
        });
        for t in [dawn(), ThemeTokens::fallback(), lunaperche] {
            let css = gtk_css(&t);
            let body = rule(&css, ".pane.pane-focused");
            assert!(body.contains(&format!("\n    outline: 2px solid {};", t.muted.hex())), "{body}");
            assert!(t.muted.contrast(t.bg) >= 3.0, "the focus outline is not visible on bg");
            // Never a border: that would resize the editor's grid on every focus change.
            assert!(!body.contains("border"), "{body}");
        }
    }

    /// Winning the cascade is per property, not per rule: a property this stylesheet leaves unset
    /// still comes from a lower provider. A user `gtk.css` that draws the divider as a
    /// `background-image` (Nordic does: `image(#1f232b)`) paints over our `background-color`.
    #[test]
    fn the_divider_clears_any_background_image_a_user_theme_draws_it_with() {
        let css = gtk_css(&ThemeTokens::fallback());
        let body = rule(&css, "paned.content-area > separator");
        assert!(body.contains("\n    background-image: none;"), "{body}");
    }
}
