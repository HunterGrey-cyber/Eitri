//! Registers plugin panels with `id`, `title`, `position`, and `content`.
//! `content` must be `{ type = "webview", url = "..." }` in v1. It is validated here
//! so invalid content fails before reaching GTK.
//!
//! The registry holds only Lua panels. Every window has editor and agent modules;
//! each Lua panel adds a module placed by its `position` (`PanelSlot::placement`).
//! `main.rs` turns this registry into `ModuleDecl`s in the shared layout.
//!
//! `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, and `resolve_panel_url` live
//! in the core Lua panel module and do not touch GTK or WebKit. This module keeps
//! `PanelEntry`, which holds a `gtk4::Widget`, and `install`, which builds a
//! `webkit6::WebView`.

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
    #[allow(dead_code)] // Reserved for tray chip labels.
    pub(crate) title: String,
    /// Where it goes the first time (`PanelSlot::placement`), nothing more.
    pub(crate) slot: PanelSlot,
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
