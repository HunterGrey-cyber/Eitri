//! Registers plugin panels with `id`, `title`, `position`, and `content`.
//! `content` must be `{ type = "webview", url = "..." }` in v1. It is validated here
//! so invalid content fails before reaching GTK.
//!
//! Rust also calls `PanelRegistry::register` directly for the two built-in panels.
//! `install()` checks the Lua content type and calls the same `register` function,
//! so built-in and plugin panels use the same registration path.

use gtk4::prelude::*;
use mlua::{Lua, Table};
use webkit6::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// The two named layout slots a panel can claim in v1. There are no tabs or multiple
/// panels per slot: registering in an occupied slot replaces its panel and logs a warning.
/// `main` is the left, primary pane containing the editor; `side` is the right,
/// secondary pane containing the agent placeholder. A plugin can replace either
/// built-in panel by registering in its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PanelSlot {
    Main,
    Side,
}

impl PanelSlot {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "main" => Some(Self::Main),
            "side" => Some(Self::Side),
            _ => None,
        }
    }
}

pub(crate) struct PanelEntry {
    pub(crate) id: String,
    #[allow(dead_code)] // read by a future panel-listing command/UI; not consumed by this plan
    pub(crate) title: String,
    pub(crate) widget: gtk4::Widget,
}

#[derive(Default)]
pub(crate) struct PanelRegistry {
    slots: HashMap<PanelSlot, PanelEntry>,
}

impl PanelRegistry {
    pub(crate) fn register(&mut self, slot: PanelSlot, entry: PanelEntry) {
        if let Some(prev) = self.slots.insert(slot, entry) {
            eprintln!(
                "[lua] panel slot {slot:?} already held '{}' -- replaced by new registration",
                prev.id
            );
        }
    }

    pub(crate) fn get(&self, slot: PanelSlot) -> Option<&PanelEntry> {
        self.slots.get(&slot)
    }
}

#[derive(Debug)]
pub(crate) struct ParsedPanelSpec {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) slot: PanelSlot,
    pub(crate) url: String,
}

/// Pure validation of a `neovibe.panel.register` call's argument table -- no GTK/WebKit touched
/// here, which is exactly why this (unlike the real widget construction below) gets a direct
/// `cargo test`.
pub(crate) fn parse_panel_spec(spec: &Table) -> mlua::Result<ParsedPanelSpec> {
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
            "neovibe.panel.register: unknown position '{position}' -- must be 'main' or 'side'"
        ))
    })?;
    Ok(ParsedPanelSpec { id, title, slot, url })
}

/// Resolves a Lua-registered webview panel's `url` field against `<config_dir>/panels/`, per
/// the plugin-loading convention (`~/.config/neovibe/panels/*.html`). A bare relative filename
/// resolves there; anything containing a URI scheme (`file://`, `http://`, `https://`) or
/// starting with `/` is used exactly as given.
pub(crate) fn resolve_panel_url(config_dir: &std::path::Path, raw: &str) -> String {
    if raw.contains("://") || raw.starts_with('/') {
        raw.to_string()
    } else {
        let path = config_dir.join("panels").join(raw);
        format!("file://{}", path.display())
    }
}

pub(crate) fn install(
    lua: &Lua,
    neovibe: &Table,
    registry: Rc<RefCell<PanelRegistry>>,
    config_dir: PathBuf,
) -> mlua::Result<()> {
    let panel_table = lua.create_table()?;
    let register_fn = lua.create_function(move |_, spec: Table| {
        let parsed = parse_panel_spec(&spec)?;
        let resolved_url = resolve_panel_url(&config_dir, &parsed.url);
        let webview = webkit6::WebView::new();
        webview.load_uri(&resolved_url);
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        registry.borrow_mut().register(
            parsed.slot,
            PanelEntry { id: parsed.id, title: parsed.title, widget: webview.upcast() },
        );
        Ok(())
    })?;
    panel_table.set("register", register_fn)?;
    neovibe.set("panel", panel_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
