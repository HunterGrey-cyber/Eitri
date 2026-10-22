//! Lua panel validation, URL resolution and widgetless registration order. The shell builds the
//! views and keeps their crash guards separately, so registration rules need no display.

use mlua::Table;

use crate::layout::{ModuleId, Placement};

/// A Lua panel's declaration, independent of its host view.
pub struct PanelEntry {
    pub module: ModuleId,
    pub id: String,
    pub title: String,
    pub slot: PanelSlot,
    pub key: Option<String>,
    pub url: String,
}

/// Lua panels in the registration order used for their initial placement.
#[derive(Default)]
pub struct PanelRegistry {
    entries: Vec<PanelEntry>,
}

impl PanelRegistry {
    /// Replacing an id moves it last; different ids sharing a position both stay.
    pub fn register(&mut self, entry: PanelEntry) {
        if let Some(i) = self.entries.iter().position(|e| e.id == entry.id) {
            eprintln!(
                "[lua] panel '{}' registered again -- replaced by the new registration",
                entry.id
            );
            self.entries.remove(i);
        }
        self.entries.push(entry);
    }

    pub fn entries(&self) -> &[PanelEntry] {
        &self.entries
    }
}

/// A Lua panel's `position`: `main`, `side` or `bottom`.
///
/// This is a first-launch placement hint. The layout is a tree of modules, and
/// [`PanelSlot::placement`] says where a panel goes the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelSlot {
    Main,
    Side,
    Bottom,
}

impl PanelSlot {
    /// `side` goes right of the whole root and `bottom` below it; `main` keeps its visual meaning
    /// by taking the editor's leaf, with the editor hidden, not gone.
    pub fn placement(self) -> Placement {
        match self {
            PanelSlot::Main => Placement::InPlaceOfEditor,
            PanelSlot::Side => Placement::RightOfRoot,
            PanelSlot::Bottom => Placement::BelowRoot,
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
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
    /// `key`, as written: the panel's module key after `Ctrl+a`. Checked by
    /// `layout::ModuleKeys::build` once every panel has registered, because a clash between two
    /// panels can only be seen then -- and a clash, or a string that is not one key, is a startup
    /// failure naming it. A `key` that is not a string at all (`key = true`) is caught earlier, by
    /// mlua's conversion here: `eitri.panel.register` itself fails, which, like every `init.lua`
    /// error, is logged and leaves that panel out. A number is taken as its text (`key = 1` is `'1'`).
    pub key: Option<String>,
}

/// Pure validation of an `eitri.panel.register` call's argument table -- no GTK/WebKit touched
/// here, which is exactly why this (unlike the real widget construction below) gets a direct
/// `cargo test`.
pub fn parse_panel_spec(spec: &Table) -> mlua::Result<ParsedPanelSpec> {
    let id: String = spec.get("id")?;
    // The panel's module is `lua:<id>`, and `ModuleId::parse` rejects `lua:` alone.
    // An empty id would be dropped from saved state and reset the panel's placement on every launch.
    // Reject it here, just like an unknown `position`.
    if id.is_empty() {
        return Err(mlua::Error::RuntimeError(
            "eitri.panel.register: id must not be empty".to_string(),
        ));
    }
    if ModuleId::parse(&id).is_ok_and(|module| module.kind().has_instances() && module.number() > 1) {
        return Err(mlua::Error::RuntimeError(format!(
            "eitri.panel.register: id '{id}' is the name of an instance (terminal#2..#9 and agent#2..#9 are reserved)"
        )));
    }
    let title: String = spec.get("title")?;
    let position: String = spec.get("position")?;
    let content: Table = spec.get("content")?;
    let content_type: String = content.get("type")?;
    if content_type != "webview" {
        return Err(mlua::Error::RuntimeError(format!(
            "eitri.panel.register: unsupported content.type '{content_type}' -- v1 only supports 'webview'"
        )));
    }
    let url: String = content.get("url")?;
    let key: Option<String> = spec.get("key")?;
    let slot = PanelSlot::parse(&position).ok_or_else(|| {
        mlua::Error::RuntimeError(format!(
            "eitri.panel.register: unknown position '{position}' -- must be 'main', 'side' or 'bottom'"
        ))
    })?;
    Ok(ParsedPanelSpec {
        id,
        title,
        slot,
        url,
        key,
    })
}

/// True if `raw` already begins with a URI scheme (`data:`, `file:`, `http:`, `https:`, ...) --
/// i.e. an alphanumeric (plus `+`/`-`/`.`) prefix followed by `:`, per RFC 3986's scheme
/// grammar. Deliberately not limited to schemes that also have a `//` authority component
/// (`file://`, `http://`): `data:` URIs have no authority at all (`data:text/html,<h1>...`),
/// so a `raw.contains("://")` check alone misses them and wrongly treats them as a relative
/// filename -- found via sandbox verification (a `data:text/html,...` panel URL from
/// `eitri.panel.register` resolved to a bogus `file://<config_dir>/panels/data:text/html,...`
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
/// the plugin-loading convention (`$XDG_CONFIG_HOME/eitri/panels/*.html`, default
/// `~/.config/eitri/panels/*.html`). A bare relative filename resolves there; anything already
/// carrying a URI scheme (`data:`, `file://`, `http://`, `https://`, ...) or starting with `/` is
/// used exactly as given.
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

    fn entry(id: &str, slot: PanelSlot) -> PanelEntry {
        PanelEntry {
            module: ModuleId::lua(id),
            id: id.into(),
            title: id.into(),
            slot,
            key: None,
            url: format!("file:///{id}.html"),
        }
    }

    #[test]
    fn panels_keep_their_registration_order_and_two_in_one_position_both_stay() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom));
        registry.register(entry("b", PanelSlot::Bottom));
        registry.register(entry("c", PanelSlot::Side));
        let ids: Vec<&str> = registry.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn registering_an_id_again_replaces_it_and_moves_it_last() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom));
        registry.register(entry("b", PanelSlot::Side));
        registry.register(entry("a", PanelSlot::Main));
        let seen: Vec<(&str, PanelSlot)> = registry.entries().iter().map(|e| (e.id.as_str(), e.slot)).collect();
        assert_eq!(seen, [("b", PanelSlot::Side), ("a", PanelSlot::Main)]);
        assert_eq!(registry.entries()[1].module, ModuleId::lua("a"));
    }

    #[test]
    fn a_panel_registered_twice_is_replaced_in_registration_order() {
        panels_keep_their_registration_order_and_two_in_one_position_both_stay();
        registering_an_id_again_replaces_it_and_moves_it_last();
    }

    #[test]
    fn the_registry_has_no_widget() {
        use crate::source_scan::{braced, code_only, read_required};
        let source = read_required("core/src/lua/panel.rs");
        let code = code_only(&source.text).split_whitespace().collect::<Vec<_>>().join(" ");
        let declaration = code
            .split_once("pub struct PanelEntry")
            .expect("panel entry declaration")
            .1;
        assert!(
            declaration.trim_start().starts_with('{'),
            "entry must not have generic parameters"
        );
        let body = braced(declaration, declaration.find('{').unwrap());
        let fields: Vec<_> = body[1..body.len() - 1]
            .split(',')
            .filter_map(|field| {
                field
                    .split_once(':')
                    .map(|(_, ty)| ty.chars().filter(|c| !c.is_whitespace()).collect::<String>())
            })
            .collect();
        assert_eq!(
            fields,
            ["ModuleId", "String", "String", "PanelSlot", "Option<String>", "String"]
        );
    }

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

    fn spec_with_id(lua: &Lua, id: &str) -> Table {
        let spec = valid_spec(lua);
        spec.set("id", id).unwrap();
        spec
    }

    #[test]
    fn an_id_spelled_like_an_instance_is_refused() {
        let lua = Lua::new();
        for id in ["terminal#2", "terminal#9", "agent#2", "agent#9"] {
            let err = parse_panel_spec(&spec_with_id(&lua, id)).unwrap_err().to_string();
            assert!(
                err.contains(&format!("eitri.panel.register: id '{id}' is the name of an instance")),
                "{err}"
            );
        }
        for id in ["terminal#1", "notes#2", "terminal#10"] {
            assert!(parse_panel_spec(&spec_with_id(&lua, id)).is_ok(), "{id}");
        }
    }

    #[test]
    fn parses_a_valid_webview_spec() {
        let lua = Lua::new();
        let parsed = parse_panel_spec(&valid_spec(&lua)).unwrap();
        assert_eq!(parsed.id, "test-panel");
        assert_eq!(parsed.title, "Test Panel");
        assert_eq!(parsed.slot, PanelSlot::Side);
        assert_eq!(parsed.url, "test.html");
        assert_eq!(parsed.key, None);
    }

    /// `key` is optional and kept as written. `ModuleKeys::build` validates it once every
    /// panel has registered.
    #[test]
    fn a_key_is_kept_as_written() {
        let lua = Lua::new();
        let spec: Table = lua
            .load(
                r#"
                return {
                    id = "notes", title = "Notes", position = "side", key = "N",
                    content = { type = "webview", url = "notes.html" },
                }
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(parse_panel_spec(&spec).unwrap().key.as_deref(), Some("N"));

        let spec_with = |key: &str| -> Table {
            lua.load(format!(
                r#"return {{ id = "notes", title = "Notes", position = "side", key = {key},
                            content = {{ type = "webview", url = "notes.html" }} }}"#
            ))
            .eval()
            .unwrap()
        };
        assert_eq!(parse_panel_spec(&spec_with("1")).unwrap().key.as_deref(), Some("1"));
        assert!(
            parse_panel_spec(&spec_with("true")).is_err(),
            "not a string: register fails"
        );
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
    /// `eitri.panel.register{ position = "bottom" }` errors at runtime.
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
        let dir = std::path::Path::new("/home/example/.config/eitri");
        assert_eq!(
            resolve_panel_url(dir, "my-panel.html"),
            "file:///home/example/.config/eitri/panels/my-panel.html"
        );
    }

    #[test]
    fn resolve_panel_url_passes_through_absolute_and_scheme_urls() {
        let dir = std::path::Path::new("/home/example/.config/eitri");
        assert_eq!(resolve_panel_url(dir, "https://example.com"), "https://example.com");
        assert_eq!(resolve_panel_url(dir, "/tmp/x.html"), "/tmp/x.html");
    }

    #[test]
    fn resolve_panel_url_passes_through_data_uris() {
        // `data:` URIs have no `//` authority component (unlike `file://`/`http://`), so they
        // need their own scheme-aware check -- found broken via sandbox verification (task 8).
        let dir = std::path::Path::new("/home/example/.config/eitri");
        let raw = "data:text/html,<h1>plugin panel</h1>";
        assert_eq!(resolve_panel_url(dir, raw), raw);
    }
}
