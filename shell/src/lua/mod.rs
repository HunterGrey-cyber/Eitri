//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua (see docs/canonical/neovibe_architecture_decisions.md §3). Exposes exactly four v1
//! extension points under a `neovibe` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`.

mod panel;

pub(crate) use neovibe_core::lua::command::CommandRegistry;
pub(crate) use neovibe_core::lua::panel::PanelSlot;
pub(crate) use panel::PanelRegistry;

use mlua::{Lua, Value};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) struct LuaEngine {
    lua: Lua,
    pub(crate) panels: Rc<RefCell<PanelRegistry>>,
    pub(crate) commands: Rc<RefCell<CommandRegistry>>,
    events: Rc<RefCell<neovibe_core::lua::event::EventBus>>,
    /// Kept as a field since 2026-09-21, and the comment in `new()` that said it need not be says
    /// why the reason changed: the shell itself now reads a key out of it (`agent.account`) after
    /// `init.lua` has run.
    pub(crate) config: Rc<RefCell<neovibe_core::lua::config::ConfigStore>>,
    /// `neovibe.layout.*` (modules P2): `init.lua`'s default tree, and the requests commands queue.
    pub(crate) layout: Rc<RefCell<neovibe_core::lua::layout::LayoutStore>>,
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let neovibe = lua.create_table()?;

        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let commands = Rc::new(RefCell::new(CommandRegistry::default()));
        let events = Rc::new(RefCell::new(neovibe_core::lua::event::EventBus::default()));
        // Kept as a field now, and the note this replaces is worth keeping in view: the store is
        // held alive regardless by the `get`/`set` closures `config::install` registers on the
        // `neovibe.config` table, which `lua` (a real field below) owns for as long as
        // `LuaEngine` lives. So this field exists for a reader, not for a lifetime -- `main()`
        // reads `agent.account` out of it once `init.lua` has run.
        let config = Rc::new(RefCell::new(neovibe_core::lua::config::ConfigStore::default()));

        panel::install(&lua, &neovibe, panels.clone(), config_dir)?;
        neovibe_core::lua::command::install(&lua, &neovibe, commands.clone())?;
        neovibe_core::lua::event::install(&lua, &neovibe, events.clone())?;
        neovibe_core::lua::config::install(&lua, &neovibe, config.clone())?;
        let layout = Rc::new(RefCell::new(neovibe_core::lua::layout::LayoutStore::default()));
        neovibe_core::lua::layout::install(&lua, &neovibe, layout.clone())?;

        lua.globals().set("neovibe", neovibe)?;

        Ok(Self {
            lua,
            panels,
            commands,
            events,
            config,
            layout,
        })
    }

    pub(crate) fn emit(&self, event_name: &str) {
        neovibe_core::lua::event::emit(&self.lua, &self.events, event_name, Value::Nil);
    }

    /// Same reentrancy hazard and same fix as `neovibe_core::lua::event::emit` (see that function's doc comment):
    /// the command's `action` key is cloned out of `self.commands` under a scoped borrow, which
    /// is dropped *before* the action is actually called -- so an action that itself calls
    /// `neovibe.command.register(...)` (registering another command, from inside a command's own
    /// action) doesn't hit `CommandRegistry`'s `borrow_mut()` while this function's own borrow is
    /// still held.
    pub(crate) fn invoke_command(&self, id: &str) {
        let action = {
            let commands = self.commands.borrow();
            let Some(entry) = commands.get(id) else { return };
            entry.action.clone()
        };
        let Ok(f) = self.lua.registry_value::<mlua::Function>(&action) else {
            return;
        };
        if let Err(err) = f.call::<()>(()) {
            eprintln!("[lua] command '{id}' handler error: {err}");
        }
    }

    /// Loads and executes `path` (expected: `<config_dir>/init.lua`). Never propagates a Lua
    /// error up to the caller -- a broken or missing init.lua must not crash the shell; this
    /// logs and the shell continues with whatever it registered before the error (the editor and
    /// the agent are not registered here at all: every window has them).
    pub(crate) fn load_init_file(&self, path: &std::path::Path) {
        if !path.exists() {
            println!(
                "[lua] no init.lua at {} -- continuing with built-ins only",
                path.display()
            );
            return;
        }
        match std::fs::read_to_string(path) {
            Ok(src) => {
                if let Err(err) = self.lua.load(&src).set_name(path.to_string_lossy().to_string()).exec() {
                    eprintln!("[lua] error loading {}: {err}", path.display());
                }
            }
            Err(err) => eprintln!("[lua] could not read {}: {err}", path.display()),
        }
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
