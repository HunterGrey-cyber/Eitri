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
    // StatusLine makes `chrome` the colour they were pushed toward.
    let chrome_muted = tokens.chrome_muted.hex();
    let border = tokens.border.hex();
    let hover = tokens.cursorline.hex();
    let error = tokens.error.hex();
    // The global `f` HINT's label pair (nvim's IncSearch). `hint_fg` is guarded for text against
    // `hint_bg` only (`neovibe_core::theme::tokens`), so it is used nowhere but on its own fill.
    let hint_bg = tokens.hint_bg.hex();
    let hint_fg = tokens.hint_fg.hex();
    let font = PROSE_FONT_STACK;
    // The module grid's own CSS node name, so its dividers take the rule below: it is the only
    // container with dividers since the grid replaced the two `GtkPaned`s (modules design P1).
    let grid = crate::module_grid::CSS_NAME;

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
    padding: 0 4px;
    border-radius: 3px;
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

{grid}.content-area > separator {{
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

/* The tray (modules P2, `shell::tray`): a chip for each module not on screen. Quiet like the
   project name until the agent's asks (`agent ⚑N`), which takes the bar's full text colour. Its
   cursor is `.topbar-item:focus`, below -- and must stay below: `.tray-chip.attention` is as
   specific, so the later rule wins, and a focused chip asking for the user must still be drawn as
   the cursor. */
.tray-chip {{
    background-color: transparent;
    background-image: none;
    border: 1px solid {border};
    box-shadow: none;
    color: {chrome_muted};
    min-height: 22px;
    padding: 0 8px;
    border-radius: 4px;
    font-size: 12px;
}}

.tray-chip.attention {{
    color: {chrome_fg};
    font-weight: 700;
}}

/* The prefix strip (modules P2, `shell::prefix_strip`): the module keys and verbs while `Ctrl+a`
   waits, a hidden module in the muted colour -- a token, never opacity, which this project has had
   to take back three times. `·` between two runs is muted too. The right margin keeps the last run
   off the project name (the GUI pass's C1: `H J K L swapproj`): 10px, about one run-to-run gap,
   and with no `·` before the name it reads as the end of the list rather than as one more run. It
   adds those 10px to the armed bar's minimum width; while the prefix does not wait the strip is
   hidden, margin and all. */
.prefix-strip {{
    color: {chrome_fg};
    font-size: 12px;
    margin-right: 10px;
}}

.prefix-strip .dimmed {{
    color: {chrome_muted};
}}

.prefix-strip .strip-dot {{
    color: {chrome_muted};
}}

/* The top bar's keyboard cursor. `Ctrl+k` brings focus here, and the focused item is drawn the
   way every other cursor in this window is: a solid block with the glyph knocked out. The editor's
   block is Neovide's own, the panel's is its current row's sign cell. That is the whole focus
   language: solid means the keys go here. The pair is `chrome_fg` behind `chrome` text, the
   reverse of the bar's guarded `chrome_fg`-on-`chrome` text pair, so it has the same 4.5:1.
   Placed after `.win-btn:hover` so a hovered AND focused button still shows the cursor. */
.topbar-item:focus {{
    background-color: {chrome_fg};
    color: {chrome};
}}

/* The `Ctrl+a` prefix is waiting for its next key (shell::prefix). The same solid block as the top
   bar's own cursor -- the pair is chrome_fg behind chrome text, guarded at 4.5:1 -- as the owner's
   tmux theme highlights its session name while the prefix is down. */
.topbar-app-name.prefix-armed {{
    background-color: {chrome_fg};
    color: {chrome};
}}

/* A layout verb was refused -- `Ctrl+a x` on the last module on screen (modules P2, spec §3.2): the
   same block, for 200ms (`main.rs`'s `refuse`). Its own class, so the prefix's own indicator is
   never switched off by the flash ending. */
.topbar-app-name.refused {{
    background-color: {chrome_fg};
    color: {chrome};
}}

/* The toast (modules P2, `shell::toast`): a hidden chat's new permission card, over everything in
   the top right. The top bar's own guarded pair, so it reads the same in every mode, Immersive
   included, where it is the only thing that says so. */
.module-toast {{
    background-color: {chrome};
    color: {chrome_fg};
    border: 1px solid {border};
    border-radius: 6px;
    padding: 6px 10px;
    font-size: 12px;
}}

/* The window-close y/n (session tabs spec §3.5, D11 A) and `prefix x`'s kill y/n (2026-09-26,
   Task 5): `.module-toast`'s own colours, `shell::close_prompt`'s own widget. Sized to sit inside
   the top bar's own strip, where the prefix strip just drew (`close_prompt::prompt_origin`). */
.close-prompt {{
    background-color: {chrome};
    color: {chrome_fg};
    border: 1px solid {border};
    border-radius: 4px;
    padding: 2px 8px;
    font-size: 12px;
}}

/* The global f HINT's GTK labels -- top bar items, the editor pane, a bottom plugin pane
   (spec 2026-09-19-global-hint-design.md §3.5). The panel draws its own, in the same pair. They sit
   in the window's hint overlay and never take a click (`can_target(false)`). The fill is in this
   same rule because `hint_fg` is only guarded against `hint_bg`. */
.hint-label {{
    background-color: {hint_bg};
    color: {hint_fg};
    font-family: monospace;
    font-size: 11px;
    font-weight: 700;
    padding: 1px 4px;
    border-radius: 3px;
}}

/* A label the typed prefix can no longer reach recedes, as the panel's `.hint-off` does. */
.hint-label.hint-off {{
    opacity: 0.3;
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

    /// Reloads the stylesheet with `tokens`, then has every widget under `root` that GTK would leave
    /// on the previous stylesheet take this one ([`super::restyle::restyle_unrealized`]; modules P2's
    /// GUI pass, D1: a tray chip first shown after startup kept the fallback's colours). `root` is
    /// the window: every widget this stylesheet paints is under it.
    pub(crate) fn update(&self, tokens: &ThemeTokens, root: &gtk4::Widget) {
        self.provider.load_from_string(&gtk_css(tokens));
        super::restyle::restyle_unrealized(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neovibe_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};

    fn dawn() -> ThemeTokens {
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [(
                "Normal".to_string(),
                HlAttrs {
                    fg: Some(0x575279),
                    bg: Some(0xfaf4ed),
                    reverse: false,
                },
            )]
            .into(),
            options: NvimOptions {
                background: "light".into(),
                guifont: String::new(),
                colors_name: "rose-pine".into(),
            },
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

    /// The body of the rule whose selector is exactly `selector` (`.win-btn`, not `.win-btn:hover`).
    fn rule<'a>(css: &'a str, selector: &str) -> &'a str {
        let start = css
            .find(&format!("\n{selector} {{"))
            .unwrap_or_else(|| panic!("no rule {selector}"));
        let body = &css[start..];
        &body[..body.find('}').unwrap()]
    }

    #[test]
    fn text_on_chrome_is_paired_with_chrome_not_with_the_window_background() {
        // lunaperche: reversed StatusLine, no Function. With bg-guarded tokens the (since deleted)
        // status bar's "NORMAL" came out at 1.00:1 on chrome; the top bar's text is the same pair.
        let t = ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                (
                    "Normal".to_string(),
                    HlAttrs {
                        fg: Some(0xc6c6c6),
                        bg: Some(0x000000),
                        reverse: false,
                    },
                ),
                (
                    "StatusLine".to_string(),
                    HlAttrs {
                        fg: None,
                        bg: None,
                        reverse: true,
                    },
                ),
                (
                    "Comment".to_string(),
                    HlAttrs {
                        fg: Some(0x949494),
                        bg: None,
                        reverse: false,
                    },
                ),
            ]
            .into(),
            options: NvimOptions {
                background: "dark".into(),
                guifont: String::new(),
                colors_name: "lunaperche".into(),
            },
        });
        let css = gtk_css(&t);
        for (selector, colour, min) in [
            (".module-toast", t.chrome_fg, 4.5),
            (".close-prompt", t.chrome_fg, 4.5),
            (".tray-chip", t.chrome_muted, 4.5),
            (".tray-chip.attention", t.chrome_fg, 4.5),
            (".prefix-strip", t.chrome_fg, 4.5),
            (".prefix-strip .dimmed", t.chrome_muted, 4.5),
            (".prefix-strip .strip-dot", t.chrome_muted, 4.5),
            (".topbar-app-name", t.chrome_fg, 4.5),
            (".topbar-project-name", t.chrome_muted, 4.5),
            (".win-btn", t.chrome_muted, 4.5),
        ] {
            let body = rule(&css, selector);
            assert!(
                body.contains(&format!("\n    color: {};", colour.hex())),
                "{selector}: {body}"
            );
            assert!(colour.contrast(t.chrome) >= min, "{selector} is unreadable on chrome");
        }
    }

    /// Focus is shown by cursors, never by chrome around the panes (2026-09-19). The owner found a
    /// frame ugly and a line under the pane a stopgap; any rule on `.pane` would also risk taking
    /// layout space and resizing nvim's grid on every focus change. The top bar's own cursor is the
    /// guarded chrome pair, reversed.
    #[test]
    fn focus_is_a_cursor_not_a_frame() {
        for t in [dawn(), ThemeTokens::fallback()] {
            let css = gtk_css(&t);
            assert!(
                !css.contains("\n.pane {") && !css.contains("pane-focused"),
                "no pane focus chrome"
            );
            assert!(!css.contains("outline:"), "no outline");
            let item = rule(&css, ".topbar-item:focus");
            assert!(
                item.contains(&format!("background-color: {};", t.chrome_fg.hex())),
                "{item}"
            );
            assert!(item.contains(&format!("color: {};", t.chrome.hex())), "{item}");
            assert!(t.chrome_fg.contrast(t.chrome) >= 4.5);
            assert!(
                css.find(".win-btn:hover").unwrap() < css.find(".topbar-item:focus").unwrap(),
                "hover must not win"
            );
        }
    }

    /// A focused chip is drawn as the cursor even while it asks for the user: `.tray-chip.attention`
    /// is as specific as `.topbar-item:focus`, so it has to come first.
    #[test]
    fn a_focused_chip_is_the_cursor_even_while_it_asks() {
        let css = gtk_css(&ThemeTokens::fallback());
        assert!(css.find("\n.tray-chip.attention {").unwrap() < css.find("\n.topbar-item:focus {").unwrap());
        assert!(css.find("\n.tray-chip {").unwrap() < css.find("\n.topbar-item:focus {").unwrap());
    }

    /// The GUI pass's C1 (2026-09-24): armed, the strip's last run touched the project name
    /// (`H J K L swapproj`). The strip ends in a margin of its own -- on the strip, which is hidden
    /// while the prefix does not wait, so the unarmed bar is not moved by it.
    #[test]
    fn the_strip_keeps_off_the_project_name() {
        let css = gtk_css(&ThemeTokens::fallback());
        let strip = rule(&css, ".prefix-strip");
        assert!(strip.contains("\n    margin-right: 10px;"), "{strip}");
        let name = rule(&css, ".topbar-project-name");
        assert!(
            !name.contains("margin"),
            "the name must not move when the prefix is not armed: {name}"
        );
    }

    /// `theme::restyle` toggles a class to make GTK recompute a widget's style; a rule naming that
    /// class would paint every widget it touches for the moment it is set.
    #[test]
    fn no_rule_names_the_restyle_class() {
        for t in [dawn(), ThemeTokens::fallback(), lunaperche()] {
            assert!(!gtk_css(&t).contains(crate::theme::restyle::RESTYLE_CLASS));
        }
    }

    /// A web module's `WebHost` exists only to hold its `WebView` at (0,0) (`web_host`'s module doc)
    /// and must add nothing around it: the host's rectangle is the module's and the `WebView`'s. No
    /// rule here names the host's node, so neovibe's own stylesheet gives it no padding, border or
    /// background of its own.
    #[test]
    fn no_rule_names_the_web_host() {
        for t in [dawn(), ThemeTokens::fallback(), lunaperche()] {
            assert!(!gtk_css(&t).contains(crate::web_host::CSS_NAME));
        }
    }

    /// The refusal flash is the prefix indicator's block (spec §3.2), and like it wins over the
    /// app name's own colours by coming later.
    #[test]
    fn the_refusal_flash_is_the_prefix_indicators_block() {
        for t in [dawn(), ThemeTokens::fallback()] {
            let css = gtk_css(&t);
            let refused = rule(&css, ".topbar-app-name.refused");
            assert!(
                refused.contains(&format!("background-color: {};", t.chrome_fg.hex())),
                "{refused}"
            );
            assert!(refused.contains(&format!("color: {};", t.chrome.hex())), "{refused}");
            assert!(css.find("\n.topbar-app-name {").unwrap() < css.find("\n.topbar-app-name.refused").unwrap());
        }
    }

    /// The `Ctrl+a` prefix's indicator (`shell::prefix`, spec §3.1): the app-name label goes solid
    /// while it waits, the same pair as the top bar's own focus cursor. Placed after the base
    /// `.topbar-app-name` rule so it wins the cascade (equal specificity, later wins).
    #[test]
    fn the_prefix_indicator_reuses_the_cursor_pair_and_wins_the_cascade() {
        for t in [dawn(), ThemeTokens::fallback()] {
            let css = gtk_css(&t);
            let armed = rule(&css, ".topbar-app-name.prefix-armed");
            assert!(
                armed.contains(&format!("background-color: {};", t.chrome_fg.hex())),
                "{armed}"
            );
            assert!(armed.contains(&format!("color: {};", t.chrome.hex())), "{armed}");
            assert!(
                css.find("\n.topbar-app-name {").unwrap() < css.find("\n.topbar-app-name.prefix-armed {").unwrap(),
                "the armed rule must come after the base rule"
            );
        }
    }

    fn lunaperche() -> ThemeTokens {
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                (
                    "Normal".to_string(),
                    HlAttrs {
                        fg: Some(0xc6c6c6),
                        bg: Some(0x000000),
                        reverse: false,
                    },
                ),
                (
                    "StatusLine".to_string(),
                    HlAttrs {
                        fg: None,
                        bg: None,
                        reverse: true,
                    },
                ),
                (
                    "Comment".to_string(),
                    HlAttrs {
                        fg: Some(0x949494),
                        bg: None,
                        reverse: false,
                    },
                ),
            ]
            .into(),
            options: NvimOptions {
                background: "dark".into(),
                guifont: String::new(),
                colors_name: "lunaperche".into(),
            },
        })
    }

    /// rose-pine dawn with its real IncSearch (`#faf4ed` on `#d7827e`, 2.60:1 as written, and
    /// neither of Normal's colours reaches 4.5:1 on that fill). The fixtures above carry no
    /// IncSearch at all, so on them the pair is Normal's reversed and only re-checks Normal's own
    /// contrast.
    fn dawn_with_incsearch() -> ThemeTokens {
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                (
                    "Normal".to_string(),
                    HlAttrs {
                        fg: Some(0x575279),
                        bg: Some(0xfaf4ed),
                        reverse: false,
                    },
                ),
                (
                    "IncSearch".to_string(),
                    HlAttrs {
                        fg: Some(0xfaf4ed),
                        bg: Some(0xd7827e),
                        reverse: false,
                    },
                ),
            ]
            .into(),
            options: NvimOptions {
                background: "light".into(),
                guifont: String::new(),
                colors_name: "rose-pine".into(),
            },
        })
    }

    /// The global `f` HINT's GTK labels (top-bar items, the editor, a Lua panel) are the IncSearch
    /// pair, the same as the panel's. `hint_fg` is guarded for text against `hint_bg` only, so the
    /// rule must lay its own `hint_bg` fill under the text and use no other text colour.
    #[test]
    fn a_hint_label_is_hint_fg_text_on_its_own_hint_bg_fill() {
        let incsearch = dawn_with_incsearch();
        assert_eq!(
            incsearch.hint_bg.hex(),
            "#d7827e",
            "the pair really comes from IncSearch here"
        );
        for t in [dawn(), ThemeTokens::fallback(), lunaperche(), incsearch] {
            let css = gtk_css(&t);
            let body = rule(&css, ".hint-label");
            assert!(
                body.contains(&format!("\n    background-color: {};", t.hint_bg.hex())),
                "{body}"
            );
            let text_colours: Vec<&str> = body.lines().filter(|l| l.trim_start().starts_with("color:")).collect();
            let expected = format!("    color: {};", t.hint_fg.hex());
            assert_eq!(text_colours, vec![expected.as_str()], "{body}");
            assert!(t.hint_fg.contrast(t.hint_bg) >= 4.5, "hint_fg is unreadable on hint_bg");
        }
    }

    /// Winning the cascade is per property, not per rule: a property this stylesheet leaves unset
    /// still comes from a lower provider. A user `gtk.css` that draws the divider as a
    /// `background-image` (Nordic does: `image(#1f232b)`) paints over our `background-color`.
    #[test]
    fn the_divider_clears_any_background_image_a_user_theme_draws_it_with() {
        let css = gtk_css(&ThemeTokens::fallback());
        let body = rule(
            &css,
            &format!("{}.content-area > separator", crate::module_grid::CSS_NAME),
        );
        assert!(body.contains("\n    background-image: none;"), "{body}");
    }
}
