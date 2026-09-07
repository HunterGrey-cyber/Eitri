use mlua::{Lua, Table};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PanelSlot {
    Main,
    Side,
}

pub(crate) struct PanelEntry {
    pub(crate) id: String,
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
    _lua: &Lua,
    _neovibe: &Table,
    _registry: Rc<RefCell<PanelRegistry>>,
    _config_dir: PathBuf,
) -> mlua::Result<()> {
    Ok(())
}
