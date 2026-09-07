//! The shell's embedded Lua extension kernel -- a separate, independent runtime from Neovim's
//! own internal Lua (see docs/canonical/neovibe_architecture_decisions.md §3). Exposes exactly four v1
//! extension points under a `neovibe` global table: `panel.register`, `command.register`, `on`,
//! `config.get`/`config.set`.

mod command;
mod config;
mod event;
mod panel;

pub(crate) use command::CommandRegistry;
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
}

impl LuaEngine {
    pub(crate) fn new(config_dir: PathBuf) -> mlua::Result<Self> {
        let lua = Lua::new();
        let neovibe = lua.create_table()?;

        let panels = Rc::new(RefCell::new(PanelRegistry::default()));
        let commands = Rc::new(RefCell::new(CommandRegistry::default()));
        let events = Rc::new(RefCell::new(event::EventBus::default()));
        // Not kept as a `LuaEngine` field: `config::install` clones this `Rc` into the
        // `get`/`set` closures it registers on the `neovibe.config` table, and those closures
        // are themselves kept alive by `lua` (a real field below) for as long as `LuaEngine`
        // lives -- so a separate `LuaEngine.config` field would hold a third clone that nothing
        // ever reads, not one needed to keep the store alive.
        let config = Rc::new(RefCell::new(config::ConfigStore::default()));

        panel::install(&lua, &neovibe, panels.clone(), config_dir)?;
        command::install(&lua, &neovibe, commands.clone())?;
        event::install(&lua, &neovibe, events.clone())?;
        config::install(&lua, &neovibe, config)?;

        lua.globals().set("neovibe", neovibe)?;

        Ok(Self { lua, panels, commands, events })
    }

    pub(crate) fn emit(&self, event_name: &str) {
        event::emit(&self.lua, &self.events, event_name, Value::Nil);
    }

    /// Same reentrancy hazard and same fix as `event::emit` (see that function's doc comment):
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
        let Ok(f) = self.lua.registry_value::<mlua::Function>(&action) else { return };
        if let Err(err) = f.call::<()>(()) {
            eprintln!("[lua] command '{id}' handler error: {err}");
        }
    }

    /// Registers a built-in (non-Lua) panel through the exact same `PanelRegistry::register`
    /// function `neovibe.panel.register` calls -- this, not documentation, is what makes
    /// "built-in and plugin panels share one path" true. Bypasses the Lua-facing WebView-only
    /// content-type check entirely, since built-in panels construct their own native widget in
    /// Rust and were never going through a Lua table to begin with.
    pub(crate) fn register_builtin_panel(&self, slot: PanelSlot, entry: PanelEntry) {
        self.panels.borrow_mut().register(slot, entry);
    }

    /// Loads and executes `path` (expected: `<config_dir>/init.lua`). Never propagates a Lua
    /// error up to the caller -- a broken or missing init.lua must not crash the shell; this
    /// logs and the shell continues with whatever's already registered (the built-in panels,
    /// registered by the caller *before* this is called -- see `main.rs`'s `build_ui` ordering).
    pub(crate) fn load_init_file(&self, path: &std::path::Path) {
        if !path.exists() {
            println!("[lua] no init.lua at {} -- continuing with built-ins only", path.display());
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
