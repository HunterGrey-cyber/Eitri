//! Registers plugin panels with `id`, `title`, `position`, and `content`.
//! `content` must be `{ type = "webview", url = "..." }` in v1. It is validated here
//! so invalid content fails before reaching GTK.
//!
//! Rust also calls `PanelRegistry::register` directly for the two built-in panels.
//! `install()` checks the Lua content type and calls the same `register` function,
//! so built-in and plugin panels use the same registration path.
//!
//! `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, and `resolve_panel_url` live
//! in the core Lua panel module and do not touch GTK or WebKit. This module keeps
//! `PanelEntry`, which holds a `gtk4::Widget`, and `install`, which builds a
//! `webkit6::WebView`.

use gtk4::prelude::*;
use mlua::{Lua, Table};
use webkit6::prelude::*;
use neovibe_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

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
