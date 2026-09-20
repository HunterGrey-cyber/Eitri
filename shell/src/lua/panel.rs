//! `neovibe.panel.register({ id, title, position, content })` -- registers a plugin panel.
//! `content` must be `{ type = "webview", url = "..." }` in v1 (see the spec's "Panel content
//! model" section); this is validated here, not left to fail confusingly deep in GTK code.
//!
//! `PanelRegistry::register` is also called directly, by Rust, for the two built-in panels
//! (Task 6) -- this file's `install()` (the Lua-facing half) is a thin, content-type-checking
//! wrapper *around* the same `register` function, not a separate implementation. That's what
//! makes "built-in and plugin panels share one path" literally true.
//!
//! The pure parts -- `PanelSlot`, `ParsedPanelSpec`, `parse_panel_spec`, `resolve_panel_url` --
//! moved to `neovibe_core::lua::panel` (L2 T4): no GTK/WebKit touched there. What stays here is
//! only what actually needs a display: `PanelEntry` (holds a `gtk4::Widget`) and `install`
//! (builds a real `webkit6::WebView`).

use gtk4::prelude::*;
use mlua::{Lua, Table};
use neovibe_core::lua::panel::{parse_panel_spec, resolve_panel_url, PanelSlot};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;

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
            PanelEntry {
                id: parsed.id,
                title: parsed.title,
                widget: webview.upcast(),
            },
        );
        Ok(())
    })?;
    panel_table.set("register", register_fn)?;
    neovibe.set("panel", panel_table)?;
    Ok(())
}
