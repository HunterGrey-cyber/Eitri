//! `neovibe.panel.register({ id, title, position, content })` -- registers a plugin panel.
//! `content` must be `{ type = "webview", url = "..." }` in v1 (see the spec's "Panel content
//! model" section); this is validated here, not left to fail confusingly deep in GTK code.
//!
//! **Since the modules design's P1 the registry holds Lua panels only**
//! (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md, decision 1). The editor and the
//! agent used to be registered here too, into slots a Lua panel could take from them; now they are
//! modules every window has, and a Lua panel is one more module placed by its `position`
//! (`PanelSlot::placement`). "Built-in and plugin panels share one path" became "every module is a
//! leaf of one layout" -- `main.rs` turns this registry into `ModuleDecl`s.
//!
//! The pure parts -- `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, `resolve_panel_url` --
//! moved to `neovibe_core::lua::panel` (L2 T4): no GTK/WebKit touched there. What stays here is
//! only what actually needs a display: `PanelEntry` (holds a `gtk4::Widget`) and `install`
//! (builds a real `webkit6::WebView`).

use gtk4::prelude::*;
use mlua::{Lua, Table};
use neovibe_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;

/// One Lua panel. Generic over the widget only so the registry's rules are tested without a
/// display; the product always holds a `gtk4::Widget`.
pub(crate) struct PanelEntry<W = gtk4::Widget> {
    pub(crate) id: String,
    /// Its tray chip's and the prefix strip's name for it (modules P2).
    pub(crate) title: String,
    /// Where it goes the first time (`PanelSlot::placement`), nothing more.
    pub(crate) slot: PanelSlot,
    /// Its module key after `Ctrl+a`, as written (modules P2). `main.rs` checks every panel's
    /// together (`ModuleKeys::build`) once `init.lua` has run.
    pub(crate) key: Option<String>,
    pub(crate) widget: W,
}

/// The Lua panels, in registration order -- the order `Layout::initial` places them in.
pub(crate) struct PanelRegistry<W = gtk4::Widget> {
    entries: Vec<PanelEntry<W>>,
}

impl<W> Default for PanelRegistry<W> {
    fn default() -> Self {
        PanelRegistry { entries: Vec::new() }
    }
}

impl<W> PanelRegistry<W> {
    /// A second registration with the same `id` replaces the first, as a second registration to
    /// the same slot always did; it goes last, where it was registered. Two panels with different
    /// ids and the same `position` are both placed now -- one of them no longer disappears.
    pub(crate) fn register(&mut self, entry: PanelEntry<W>) {
        if let Some(i) = self.entries.iter().position(|e| e.id == entry.id) {
            eprintln!(
                "[lua] panel '{}' registered again -- replaced by the new registration",
                entry.id
            );
            self.entries.remove(i);
        }
        self.entries.push(entry);
    }

    pub(crate) fn entries(&self) -> &[PanelEntry<W>] {
        &self.entries
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
        registry.borrow_mut().register(PanelEntry {
            id: parsed.id,
            title: parsed.title,
            slot: parsed.slot,
            key: parsed.key,
            widget: webview.upcast(),
        });
        Ok(())
    })?;
    panel_table.set("register", register_fn)?;
    neovibe.set("panel", panel_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, slot: PanelSlot, widget: &'static str) -> PanelEntry<&'static str> {
        PanelEntry {
            id: id.to_string(),
            title: id.to_string(),
            slot,
            key: None,
            widget,
        }
    }

    #[test]
    fn panels_keep_their_registration_order_and_two_in_one_position_both_stay() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom, "a1"));
        registry.register(entry("b", PanelSlot::Bottom, "b1"));
        registry.register(entry("c", PanelSlot::Side, "c1"));
        let ids: Vec<&str> = registry.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn registering_an_id_again_replaces_it_and_moves_it_last() {
        let mut registry = PanelRegistry::default();
        registry.register(entry("a", PanelSlot::Bottom, "a1"));
        registry.register(entry("b", PanelSlot::Side, "b1"));
        registry.register(entry("a", PanelSlot::Main, "a2"));
        let seen: Vec<(&str, PanelSlot, &str)> = registry
            .entries()
            .iter()
            .map(|e| (e.id.as_str(), e.slot, e.widget))
            .collect();
        assert_eq!(seen, [("b", PanelSlot::Side, "b1"), ("a", PanelSlot::Main, "a2")]);
    }
}
