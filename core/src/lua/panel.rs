//! The GTK-free half of `neovibe.panel.register({ id, title, position, content })`: parsing and
//! validating a plugin's registration table, and resolving its `content.url` to a real URL. No
//! GTK/WebKit touched here, which is exactly why this (unlike the real widget construction) gets
//! a direct `cargo test` -- see `shell::lua::panel` for the half that builds a real
//! `webkit6::WebView` and installs this into the Lua runtime.

use mlua::Table;

/// The named layout slots a panel can claim -- deliberately not a general docking system (no
/// tabs, no arbitrary panel count per slot; a later registration to an already-occupied slot
/// replaces the earlier one, logging a warning). `main` is the left/primary pane (today: the
/// editor); `side` is the right/secondary pane (today: the agent panel); `bottom` spans the full
/// width beneath both (today: the native terminal, when `--terminal` asks for it). A plugin
/// registering to any of them will replace whatever built-in is there -- accepted as this
/// project's stated personal-tool risk tolerance, not guarded against with a "protected slots"
/// concept nobody has asked for.
///
/// `bottom` differs from the other two in one way worth knowing: it is the only slot that can be
/// **empty**. `main` and `side` always have a built-in, so the layout can assume them; the bottom
/// pane only exists when something claims it, and the window is built without a vertical split at
/// all when nothing has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelSlot {
    Main,
    Side,
    Bottom,
}

impl PanelSlot {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "main" => Some(Self::Main),
            "side" => Some(Self::Side),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct ParsedPanelSpec {
    pub id: String,
    pub title: String,
    pub slot: PanelSlot,
    pub url: String,
}

/// Pure validation of a `neovibe.panel.register` call's argument table -- no GTK/WebKit touched
/// here, which is exactly why this (unlike the real widget construction below) gets a direct
/// `cargo test`.
pub fn parse_panel_spec(spec: &Table) -> mlua::Result<ParsedPanelSpec> {
    let id: String = spec.get("id")?;
    let title: String = spec.get("title")?;
    let position: String = spec.get("position")?;
    let content: Table = spec.get("content")?;
    let content_type: String = content.get("type")?;
    if content_type != "webview" {
        return Err(mlua::Error::RuntimeError(format!(
            "neovibe.panel.register: unsupported content.type '{content_type}' -- v1 only supports 'webview'"
        )));
    }
    let url: String = content.get("url")?;
    let slot = PanelSlot::parse(&position).ok_or_else(|| {
        mlua::Error::RuntimeError(format!(
            "neovibe.panel.register: unknown position '{position}' -- must be 'main', 'side' or 'bottom'"
        ))
    })?;
    Ok(ParsedPanelSpec { id, title, slot, url })
}

/// True if `raw` already begins with a URI scheme (`data:`, `file:`, `http:`, `https:`, ...) --
/// i.e. an alphanumeric (plus `+`/`-`/`.`) prefix followed by `:`, per RFC 3986's scheme
/// grammar. Deliberately not limited to schemes that also have a `//` authority component
/// (`file://`, `http://`): `data:` URIs have no authority at all (`data:text/html,<h1>...`),
/// so a `raw.contains("://")` check alone misses them and wrongly treats them as a relative
/// filename -- found via sandbox verification (a `data:text/html,...` panel URL from
/// `neovibe.panel.register` resolved to a bogus `file://<config_dir>/panels/data:text/html,...`
/// path instead of being used as-is).
fn has_uri_scheme(raw: &str) -> bool {
    match raw.find(':') {
        Some(colon_idx) if colon_idx > 0 => raw[..colon_idx]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')),
        _ => false,
    }
}

/// Resolves a Lua-registered webview panel's `url` field against `<config_dir>/panels/`, per
/// the plugin-loading convention (`~/.config/neovibe/panels/*.html`). A bare relative filename
/// resolves there; anything already carrying a URI scheme (`data:`, `file://`, `http://`,
/// `https://`, ...) or starting with `/` is used exactly as given.
pub fn resolve_panel_url(config_dir: &std::path::Path, raw: &str) -> String {
    if has_uri_scheme(raw) || raw.starts_with('/') {
        raw.to_string()
    } else {
        let path = config_dir.join("panels").join(raw);
        format!("file://{}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn valid_spec(lua: &Lua) -> Table {
        lua.load(
            r#"
            return {
                id = "test-panel",
                title = "Test Panel",
                position = "side",
                content = { type = "webview", url = "test.html" },
            }
            "#,
        )
        .eval()
        .unwrap()
    }

    #[test]
    fn parses_a_valid_webview_spec() {
        let lua = Lua::new();
        let parsed = parse_panel_spec(&valid_spec(&lua)).unwrap();
        assert_eq!(parsed.id, "test-panel");
        assert_eq!(parsed.title, "Test Panel");
        assert_eq!(parsed.slot, PanelSlot::Side);
        assert_eq!(parsed.url, "test.html");
    }

    #[test]
    fn rejects_a_non_webview_content_type() {
        let lua = Lua::new();
        let spec: Table = lua
            .load(
                r#"
                return {
                    id = "bad-panel", title = "Bad", position = "side",
                    content = { type = "native-widget", url = "n/a" },
                }
                "#,
            )
            .eval()
            .unwrap();
        let err = parse_panel_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("unsupported content.type"));
    }

    /// `bottom` is a real slot, not only something `main.rs` reaches for internally. Without this
    /// a plugin could not claim it, and the built-in terminal would be the one panel in the app
    /// that does not actually share the plugin path.
    #[test]
    fn accepts_the_bottom_position() {
        let lua = Lua::new();
        let spec: Table = lua
            .load(
                r#"
                return {
                    id = "notes", title = "Notes", position = "bottom",
                    content = { type = "webview", url = "notes.html" },
                }
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(parse_panel_spec(&spec).unwrap().slot, PanelSlot::Bottom);
    }

    #[test]
    fn rejects_an_unknown_position() {
        let lua = Lua::new();
        let spec: Table = lua
            .load(
                r#"
                return {
                    id = "bad-position", title = "Bad", position = "top",
                    content = { type = "webview", url = "x.html" },
                }
                "#,
            )
            .eval()
            .unwrap();
        let err = parse_panel_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("unknown position"));
    }

    #[test]
    fn resolve_panel_url_joins_relative_paths_under_panels_dir() {
        let dir = std::path::Path::new("/home/example/.config/neovibe");
        assert_eq!(
            resolve_panel_url(dir, "my-panel.html"),
            "file:///home/example/.config/neovibe/panels/my-panel.html"
        );
    }

    #[test]
    fn resolve_panel_url_passes_through_absolute_and_scheme_urls() {
        let dir = std::path::Path::new("/home/example/.config/neovibe");
        assert_eq!(resolve_panel_url(dir, "https://example.com"), "https://example.com");
        assert_eq!(resolve_panel_url(dir, "/tmp/x.html"), "/tmp/x.html");
    }

    #[test]
    fn resolve_panel_url_passes_through_data_uris() {
        // `data:` URIs have no `//` authority component (unlike `file://`/`http://`), so they
        // need their own scheme-aware check -- found broken via sandbox verification (task 8).
        let dir = std::path::Path::new("/home/example/.config/neovibe");
        let raw = "data:text/html,<h1>plugin panel</h1>";
        assert_eq!(resolve_panel_url(dir, raw), raw);
    }
}
