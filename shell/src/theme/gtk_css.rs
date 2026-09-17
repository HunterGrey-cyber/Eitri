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
    // bg-guarded `muted` and the unguarded `mode_browse` are not used here: a reversed StatusLine
    // makes `chrome` the colour they were pushed toward.
    let chrome_muted = tokens.chrome_muted.hex();
    let chrome_accent = tokens.chrome_accent.hex();
    let border = tokens.border.hex();
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

.statusbar-accent {{
    color: {chrome_accent};
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
            (".statusbar-accent", t.chrome_accent, 3.0),
        ] {
            let body = rule(&css, selector);
            assert!(body.contains(&format!("\n    color: {};", colour.hex())), "{selector}: {body}");
            assert!(colour.contrast(t.chrome) >= min, "{selector} is unreadable on chrome");
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
