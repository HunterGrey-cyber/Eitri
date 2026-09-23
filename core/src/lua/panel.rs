//! The GTK-free half of `neovibe.panel.register({ id, title, position, content })`: parsing and
//! validating a plugin's registration table, and resolving its `content.url` to a real URL. No
//! GTK/WebKit touched here, which is exactly why this (unlike the real widget construction) gets
//! a direct `cargo test` -- see `shell::lua::panel` for the half that builds a real
//! `webkit6::WebView` and installs this into the Lua runtime.

use mlua::Table;

use crate::layout::Placement;

/// A Lua panel's `position`: `main`, `side` or `bottom`.
///
/// **Since the modules design's P1 this is a first-launch placement hint and nothing more**
/// (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md, decision 1, §4.5). It used to
/// be the layout itself: three fixed slots, one panel each, a later registration replacing the
/// earlier one -- including the built-in editor and agent. Now the layout is a tree of modules,
/// and [`PanelSlot::placement`] says where in it a panel goes the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelSlot {
    Main,
    Side,
    Bottom,
}

impl PanelSlot {
    /// `side` goes right of the whole root and `bottom` below it; `main` keeps its visual meaning
    /// by taking the editor's leaf, with the editor hidden, not gone (spec §4.5).
    pub fn placement(self) -> Placement {
        match self {
            PanelSlot::Main => Placement::InPlaceOfEditor,
            PanelSlot::Side => Placement::RightOfRoot,
            PanelSlot::Bottom => Placement::BelowRoot,
        }
    }

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
    // The panel's module is `lua:<id>`, and `lua:` alone names nothing `ModuleId::parse` accepts:
    // P2's state file would drop it on every read and reopen the panel at its default placement
    // on every launch. Refused here, the same way an unknown `position` is.
    if id.is_empty() {
        return Err(mlua::Error::RuntimeError(
            "neovibe.panel.register: id must not be empty".to_string(),
        ));
    }
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

    /// `bottom` is a real position. This is the only test in the workspace pinning `"bottom"` as
    /// parseable at all.
    ///
    /// **Do not delete it as terminal-era leftover.** Without it, `PanelSlot::parse`'s `"bottom"`
    /// arm and `ParsedPanelSpec`'s slot handling have no coverage, and a later refactor could drop
    /// `Bottom` from the parser and ship a green build in which
    /// `neovibe.panel.register{ position = "bottom" }` errors at runtime.
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
    fn each_position_is_a_placement_and_none_replaces_a_built_in() {
        assert_eq!(PanelSlot::Main.placement(), Placement::InPlaceOfEditor);
        assert_eq!(PanelSlot::Side.placement(), Placement::RightOfRoot);
        assert_eq!(PanelSlot::Bottom.placement(), Placement::BelowRoot);
    }

    /// An empty `id` would be the module `lua:`, which does not round-trip through
    /// `ModuleId::parse` (see `layout::module`'s tests).
    #[test]
    fn rejects_an_empty_id() {
        let lua = Lua::new();
        let spec: Table = lua
            .load(
                r#"
                return {
                    id = "", title = "Nameless", position = "side",
                    content = { type = "webview", url = "x.html" },
                }
                "#,
            )
            .eval()
            .unwrap();
        let err = parse_panel_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("id must not be empty"), "{err}");
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
