//! The GTK half of the theme: one stylesheet, reloaded in place when nvim's colours change.

use gtk4::CssProvider;

use eitri_core::theme::tokens::{ThemeTokens, PROSE_FONT_STACK};

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
    // `hint_bg` only (`eitri_core::theme::tokens`), so it is used nowhere but on its own fill.
    let hint_bg = tokens.hint_bg.hex();
    let hint_fg = tokens.hint_fg.hex();
    let font = PROSE_FONT_STACK;
    // Use the module grid's CSS node name so its dividers match the rule below.
    // The grid is the only container with dividers.
    let grid = crate::module_grid::CSS_NAME;
    // Where, inside a handle's allocation, the divider's painted line begins and ends.
    let reach = crate::module_grid::DIVIDER_REACH_PX;
    let reach_end = reach + 1;
    // A tabbed or stacked container's strip, and the class on the entry it draws.
    let strip = crate::module_grid::strips::CSS_NAME;
    let active = crate::module_grid::strips::ACTIVE_CLASS;
    // A float's bar and its grip.
    let floatbar = crate::module_grid::floatbars::BAR_CSS_NAME;
    let floatgrip = crate::module_grid::floatbars::GRIP_CSS_NAME;

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

/* `marked: <title>` while a module is marked for `swap.marked`: text only, quiet like the project
   name, never a frame on the module itself. */
.topbar-mark {{
    color: {chrome_muted};
    font-size: 12px;
    margin-right: 8px;
}}

/* The current workspace's name (`chrome::workspace_text`), only while the window has several: a
   word about the layout, quiet like the mark beside it. A button only so that `h`/`l`, HINT and a
   click reach the workspace chooser; its cursor is the top bar's item cursor, below. */
.topbar-workspace {{
    background-color: transparent;
    background-image: none;
    border: none;
    box-shadow: none;
    color: {chrome_muted};
    font-size: 12px;
    min-height: 22px;
    padding: 0 4px;
    margin-right: 8px;
    border-radius: 3px;
}}

/* The rename entry, beside the workspace name while a rename is typed: the content's own text pair
   inside one rule's border, so it reads as the one place typing goes. */
.topbar-rename {{
    background-color: {bg};
    background-image: none;
    border: 1px solid {border};
    box-shadow: none;
    color: {fg};
    caret-color: {fg};
    font-size: 12px;
    min-height: 22px;
    padding: 0 6px;
    margin-right: 8px;
    border-radius: 4px;
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
    background-color: transparent;
    background-image: none;
    min-width: 1px;
    min-height: 1px;
}}

/* The handle is allocated wider than the line it shows (`module_grid::DIVIDER_REACH_PX` on each
   side, so a pointer a pixel or two off the line still takes the drag), and only the line in the
   middle of that band is painted. */
{grid}.content-area > separator.vertical {{
    background-image: linear-gradient(to right, transparent {reach}px, {border} {reach}px,
        {border} {reach_end}px, transparent {reach_end}px);
}}

{grid}.content-area > separator.horizontal {{
    background-image: linear-gradient(to bottom, transparent {reach}px, {border} {reach}px,
        {border} {reach_end}px, transparent {reach_end}px);
}}

/* A tabbed or stacked container's strip (`module_grid::strips`): the tray chip's quiet pair for
   every entry, the bar's full text colour and weight for the one the container draws. A row is
   23px plus its 1px rule, the 24px the geometry assumes before it can measure one, so the first
   strip on screen does not move what is below it. */
{strip} {{
    background-color: {chrome};
    background-image: none;
}}

{strip} > label {{
    color: {chrome_muted};
    font-size: 12px;
    min-height: 23px;
    padding: 0 8px;
    border-bottom: 1px solid {border};
}}

{strip} > label.{active} {{
    color: {chrome_fg};
    font-weight: 700;
}}

/* A float's bar (`module_grid::floatbars`): the strip's quiet pair across the float's top edge, its
   module's title in the muted colour. A row is 23px plus its 1px rule, a strip row, which is what the
   geometry gives the bar. The grip is a 12px square in the rule's colour at the float's bottom-right
   corner. */
{floatbar} {{
    background-color: {chrome};
    background-image: none;
    border-bottom: 1px solid {border};
}}

{floatbar} > label {{
    color: {chrome_muted};
    font-size: 12px;
    min-height: 23px;
    padding: 0 8px;
}}

{floatgrip} {{
    background-color: {border};
    background-image: none;
    min-width: 12px;
    min-height: 12px;
}}

.pane-placeholder {{
    background-color: {chrome};
    color: {chrome_muted};
    font-size: 12px;
}}

.pane-placeholder.left {{
    background-color: {bg};
}}

/* The tray (`shell::tray`): a chip for each module not on screen. Quiet like the
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

/* The prefix strip (`shell::prefix_strip`): the module keys and verbs while `Ctrl+a`
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

/* The `Ctrl+a` prefix is waiting for its next key (`eitri_core::wm::keys`). The same solid block as the top
   bar's own cursor -- the pair is chrome_fg behind chrome text, guarded at 4.5:1 -- as a
   tmux theme highlights its session name while the prefix is down. */
.topbar-app-name.prefix-armed {{
    background-color: {chrome_fg};
    color: {chrome};
}}

/* A layout verb was refused -- `Ctrl+a x` on the last module on screen: the
   same block, for 200ms (`main.rs`'s `show_flash`, when the controller's chrome model asks for the
   flash). Its own class, so the prefix's own indicator is never switched off by the flash ending. */
.topbar-app-name.refused {{
    background-color: {chrome_fg};
    color: {chrome};
}}

/* The toast (`shell::toast`): a hidden chat's new permission card, over everything in
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

/* The window-close y/n and `prefix x`'s kill y/n (2026-09-26):
   `.module-toast`'s own colours, `shell::close_prompt`'s own widget. Sized to sit inside
   the top bar's own strip, where the prefix strip just drew (`eitri_core::wm::close::prompt_origin`). */
.close-prompt {{
    background-color: {chrome};
    color: {chrome_fg};
    border: 1px solid {border};
    border-radius: 4px;
    padding: 2px 8px;
    font-size: 12px;
}}

/* The global f HINT's GTK labels -- top bar items, the editor pane, a bottom plugin pane
   -- are styled here. The panel draws its own, in the same pair. They sit
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

/* `select.number`'s digits: the HINT label's pair, large enough to read from across the window,
   one in the middle of each module. */
.hint-label.hint-number {{
    font-size: 32px;
    padding: 4px 14px;
    border-radius: 6px;
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
        // convention, because every colour in this window is meant to come from nvim (GTK's default
        // Adwaita appearance is suppressed). At `_APPLICATION` a foreign `~/.config/gtk-4.0/gtk.css` (a real one exists
        // on this project's own dev machine) would win over `.win-btn`/the paned divider/etc, which
        // is not "GTK providing windowing only" — it's a second, uncontrolled theme leaking through
        // the one this crate exists to paint. A GUI pass checks this against a real `gtk.css`.
        gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
        ThemeCss { provider }
    }

    /// Reloads the stylesheet with `tokens` and calls [`super::restyle::restyle_unrealized`]
    /// so widgets GTK would leave on the previous stylesheet also take the new colours.
    /// `root` is the window containing every widget this stylesheet paints.
    pub(crate) fn update(&self, tokens: &ThemeTokens, root: &gtk4::Widget) {
        self.provider.load_from_string(&gtk_css(tokens));
        apply_prefer_dark(prefers_dark(tokens));
        super::restyle::restyle_unrealized(root);
    }
}

/// Whether the widgets GTK draws itself (menus, popovers, scrollbars, the file chooser) should use
/// their dark variant: they follow the editor's background, not the desktop's preference, so a light
/// colorscheme on a dark desktop does not leave dark dialogs beside a light window.
pub(crate) fn prefers_dark(tokens: &ThemeTokens) -> bool {
    tokens.color_scheme == "dark"
}

/// Sets `gtk-application-prefer-dark-theme` on the process's settings when it differs from what is
/// set, and prints one line per change so a pass can read it. This runs on every theme packet, so
/// an unchanged value must not write the property (each write re-resolves every widget's style).
fn apply_prefer_dark(dark: bool) {
    let Some(settings) = gtk4::Settings::default() else {
        return;
    };
    if settings.is_gtk_application_prefer_dark_theme() != dark {
        settings.set_gtk_application_prefer_dark_theme(dark);
        eprintln!("[theme] prefer-dark={dark}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};

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
                cmdheight: None,
            },
        })
    }

    #[test]
    fn prefers_dark_follows_nvims_background() {
        assert!(
            prefers_dark(&ThemeTokens::fallback()),
            "the fallback is nvim's default dark scheme"
        );
        assert!(!prefers_dark(&dawn()), "a light background is not dark");
        let mut dark = dawn();
        dark.color_scheme = "dark";
        assert!(prefers_dark(&dark));
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
                cmdheight: None,
            },
        });
        let css = gtk_css(&t);
        for (selector, colour, min) in [
            (".module-toast", t.chrome_fg, 4.5),
            (".close-prompt", t.chrome_fg, 4.5),
            (".tray-chip", t.chrome_muted, 4.5),
            (".tray-chip.attention", t.chrome_fg, 4.5),
            ("constrip > label", t.chrome_muted, 4.5),
            ("constrip > label.active", t.chrome_fg, 4.5),
            ("floatbar > label", t.chrome_muted, 4.5),
            (".prefix-strip", t.chrome_fg, 4.5),
            (".prefix-strip .dimmed", t.chrome_muted, 4.5),
            (".prefix-strip .strip-dot", t.chrome_muted, 4.5),
            (".topbar-app-name", t.chrome_fg, 4.5),
            (".topbar-project-name", t.chrome_muted, 4.5),
            (".topbar-mark", t.chrome_muted, 4.5),
            (".topbar-workspace", t.chrome_muted, 4.5),
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

    /// Focus is shown by cursors, never by chrome around the panes. The design considers a
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
    /// rule here names the host's node, so Eitri's own stylesheet gives it no padding, border or
    /// background of its own.
    #[test]
    fn no_rule_names_the_web_host() {
        for t in [dawn(), ThemeTokens::fallback(), lunaperche()] {
            assert!(!gtk_css(&t).contains(crate::web_host::CSS_NAME));
        }
    }

    /// The refusal flash is the prefix indicator's block, and like it wins over the
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

    /// The `Ctrl+a` prefix's indicator (`eitri_core::wm::keys`): the app-name label goes solid
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
                cmdheight: None,
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
                cmdheight: None,
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

    /// The handle is wider than the line it shows, so the line is painted by a gradient whose
    /// stops sit where the handle's middle is, for both orientations alike.
    #[test]
    fn both_divider_orientations_paint_only_the_line_in_the_middle_of_the_handle() {
        let css = gtk_css(&ThemeTokens::fallback());
        let reach = crate::module_grid::DIVIDER_REACH_PX;
        let border = ThemeTokens::fallback().border.hex();
        for (orientation, direction) in [("vertical", "to right"), ("horizontal", "to bottom")] {
            let body = rule(
                &css,
                &format!(
                    "{}.content-area > separator.{orientation}",
                    crate::module_grid::CSS_NAME
                ),
            );
            let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let expected = format!(
                "linear-gradient({direction}, transparent {reach}px, {border} {reach}px, {border} {}px, transparent {}px)",
                reach + 1,
                reach + 1
            );
            assert!(flat.contains(&expected), "{orientation}: {flat}");
        }
    }
}
