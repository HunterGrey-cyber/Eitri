//! The shell's embedded Lua extension kernel runs independently of the editor's internal Lua
//! runtime. Its global extension table exposes panel registration (`panel.register`), command
//! registration (`command.register`), events (`on`) and configuration
//! (`config.get`/`config.set`).

mod command;
mod config;
mod event;
mod panel;

#[allow(unused_imports)]
pub(crate) use command::{CommandEntry, CommandRegistry};
#[allow(unused_imports)]
pub(crate) use panel::{PanelEntry, PanelRegistry, PanelSlot};

use mlua::{Lua, Value};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) struct LuaEngine {
    lua: Lua,
    pub(crate) panels: Rc<RefCell<PanelRegistry>>,
    pub(crate) commands: Rc<RefCell<CommandRegistry>>,
    events: Rc<RefCell<event::EventBus>>,
    config: Rc<RefCell<config::ConfigStore>>,
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let neovibe = lua.create_table()?;

        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let commands = Rc::new(RefCell::new(CommandRegistry::default()));
        let events = Rc::new(RefCell::new(event::EventBus::default()));
        let config = Rc::new(RefCell::new(config::ConfigStore::default()));

        panel::install(&lua, &neovibe, panels.clone(), config_dir)?;
        command::install(&lua, &neovibe, commands.clone())?;
        event::install(&lua, &neovibe, events.clone())?;
        config::install(&lua, &neovibe, config.clone())?;

        lua.globals().set("neovibe", neovibe)?;

        Ok(Self { lua, panels, commands, events, config })
    }

    pub(crate) fn emit(&self, event_name: &str) {
        event::emit(&self.lua, &self.events.borrow(), event_name, Value::Nil);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_and_exposes_an_empty_neovibe_table() {
        let engine = LuaEngine::new(PathBuf::from("/tmp/neovibe-test-config")).unwrap();
        let ty: String = engine.lua.load("return type(neovibe)").eval().unwrap();
        assert_eq!(ty, "table");
    }
}
