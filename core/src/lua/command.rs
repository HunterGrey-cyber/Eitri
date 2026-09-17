//! `neovibe.command.register({ id, title, keybinding, action })`. `keybinding` is optional --
//! a command with none is still invocable (e.g. from a future command palette), just not bound
//! to any accelerator. The actual GTK `gio::SimpleAction`/accelerator wiring lives in
//! `main.rs` (Task 7), not here -- this module only owns the pure id/title/keybinding/callback
//! bookkeeping, which is why (unlike `panel.rs`'s real widget construction) it's fully
//! unit-testable: `mlua::Lua` itself needs no display.

use mlua::{Lua, RegistryKey, Table};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `shell/src/main.rs` reads `keybinding` off every entry `iter()` yields, and
/// `LuaEngine::invoke_command` (`shell/src/lua/mod.rs`) reads `action`. `title` is read by
/// neither yet (`#[allow(dead_code)]` below is original, not new) and stays `pub(crate)`.
pub struct CommandEntry {
    #[allow(dead_code)] // read by a future command-palette UI; not consumed by this plan
    pub(crate) title: String,
    pub keybinding: Option<String>,
    // `Rc<RegistryKey>`, not a bare `RegistryKey`: `RegistryKey` doesn't implement `Clone`
    // (verified against the installed mlua 0.12.1's `src/types/registry_key.rs`), and
    // `LuaEngine::invoke_command` needs to clone the handle *out* of a borrowed
    // `CommandRegistry` before dropping that borrow, to avoid a reentrant `BorrowMutError` if
    // the action itself calls `neovibe.command.register(...)`.
    pub action: Rc<RegistryKey>,
}

/// `pub`: `shell/src/lua/mod.rs` holds one behind an `Rc<RefCell<_>>` field and calls `install`.
#[derive(Default)]
pub struct CommandRegistry {
    commands: HashMap<String, CommandEntry>,
}

impl CommandRegistry {
    /// `pub(crate)`: only `install`'s own closure below calls this; nothing in `shell` registers
    /// a command directly.
    pub(crate) fn register(&mut self, id: String, entry: CommandEntry) {
        if self.commands.insert(id.clone(), entry).is_some() {
            eprintln!("[lua] command '{id}' re-registered -- replaced");
        }
    }

    /// `pub`: `LuaEngine::invoke_command` calls this.
    pub fn get(&self, id: &str) -> Option<&CommandEntry> {
        self.commands.get(id)
    }

    /// `pub`: `main.rs` iterates every registered command to wire a real GTK action for it.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &CommandEntry)> {
        self.commands.iter()
    }
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(
    lua: &Lua,
    neovibe: &Table,
    registry: Rc<RefCell<CommandRegistry>>,
) -> mlua::Result<()> {
    let command_table = lua.create_table()?;
    let register_fn = lua.create_function(move |lua, spec: Table| {
        let id: String = spec.get("id")?;
        let title: String = spec.get("title")?;
        let keybinding: Option<String> = spec.get("keybinding")?;
        let action: mlua::Function = spec.get("action")?;
        let action_key = lua.create_registry_value(action)?;
        registry
            .borrow_mut()
            .register(id, CommandEntry { title, keybinding, action: Rc::new(action_key) });
        Ok(())
    })?;
    command_table.set("register", register_fn)?;
    neovibe.set("command", command_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_a_command_and_the_action_is_callable() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(
            r#"
            action_ran = false
            neovibe.command.register({
                id = "test-command",
                title = "Test Command",
                keybinding = "<primary>t",
                action = function() action_ran = true end,
            })
            "#,
        )
        .exec()
        .unwrap();

        let borrowed = registry.borrow();
        let entry = borrowed.get("test-command").expect("command should be registered");
        assert_eq!(entry.keybinding.as_deref(), Some("<primary>t"));

        let f: mlua::Function = lua.registry_value(&entry.action).unwrap();
        f.call::<()>(()).unwrap();
        let ran: bool = lua.globals().get("action_ran").unwrap();
        assert!(ran);
    }

    #[test]
    fn keybinding_is_optional() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(
            r#"
            neovibe.command.register({ id = "no-key", title = "No Key", action = function() end })
            "#,
        )
        .exec()
        .unwrap();

        assert!(registry.borrow().get("no-key").unwrap().keybinding.is_none());
    }

    /// Regression test for the reentrancy panic the final review reproduced: a command action
    /// that itself calls `neovibe.command.register(...)` -- registering another command from
    /// inside a command's own action -- must not panic with `BorrowMutError`. This mirrors
    /// `LuaEngine::invoke_command`'s own borrow/call sequence rather than going through
    /// `LuaEngine` directly, since this module (unlike `LuaEngine`) needs no display and is
    /// fully unit-testable on its own.
    #[test]
    fn action_that_registers_another_command_does_not_panic() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(
            r#"
            neovibe.command.register({
                id = "first",
                title = "First",
                action = function()
                    neovibe.command.register({
                        id = "second",
                        title = "Second",
                        action = function() end,
                    })
                end,
            })
            "#,
        )
        .exec()
        .unwrap();

        // Mirror `LuaEngine::invoke_command`'s clone-out-then-call pattern: resolve the action
        // under a scoped borrow, drop the borrow, then call it. Before the fix (a bare
        // `RegistryKey` resolved and called while still holding `registry.borrow()`), the
        // action's own `neovibe.command.register` call would panic with `BorrowMutError`.
        let action = {
            let commands = registry.borrow();
            let entry = commands.get("first").expect("command should be registered");
            entry.action.clone()
        };
        let f: mlua::Function = lua.registry_value(&action).unwrap();
        f.call::<()>(()).unwrap();

        assert!(registry.borrow().get("second").is_some());
    }
}
