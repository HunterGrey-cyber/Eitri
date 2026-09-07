//! `neovibe.panel.register({ id, title, position, content })` -- registers a plugin panel.
//! `content` must be `{ type = "webview", url = "..." }` in v1 (see the spec's "Panel content
//! model" section); this is validated here, not left to fail confusingly deep in GTK code.
//!
//! `PanelRegistry::register` is also called directly, by Rust, for the two built-in panels
//! (Task 6) -- this file's `install()` (the Lua-facing half) is a thin, content-type-checking
//! wrapper *around* the same `register` function, not a separate implementation. That's what
//! makes "built-in and plugin panels share one path" literally true.

use gtk4::prelude::*;
use mlua::{Lua, Table};
use webkit6::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// The two named layout slots a panel can claim in v1 -- deliberately not a general docking
/// system (no tabs, no arbitrary panel count per slot; a later registration to an
/// already-occupied slot replaces the earlier one, logging a warning). `main` is the
/// left/primary pane (today: the editor); `side` is the right/secondary pane (today: the
/// agent placeholder). A plugin registering to `main` or `side` will replace a built-in panel
/// there -- accepted as this project's stated personal-tool risk tolerance (see this plan's
/// Global Constraints), not guarded against with e.g. a "protected slots" concept nobody has
/// asked for.
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
pub(crate) fn resolve_panel_url(config_dir: &std::path::Path, raw: &str) -> String {
    if has_uri_scheme(raw) || raw.starts_with('/') {
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

    #[test]
    fn resolve_panel_url_passes_through_data_uris() {
        // `data:` URIs have no `//` authority component (unlike `file://`/`http://`), so they
        // need their own scheme-aware check -- found broken via sandbox verification (task 8).
        let dir = std::path::Path::new("/home/example/.config/neovibe");
        let raw = "data:text/html,<h1>plugin panel</h1>";
        assert_eq!(resolve_panel_url(dir, raw), raw);
    }
}
