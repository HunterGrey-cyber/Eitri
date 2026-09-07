//! `neovibe.command.register({ id, title, keybinding, action })`. `keybinding` is optional --
//! a command with none is still invocable (e.g. from a future command palette), just not bound
//! to any accelerator. The actual GTK `gio::SimpleAction`/accelerator wiring lives in
//! `main.rs`, not here -- this module only owns the pure id/title/keybinding/callback
//! bookkeeping, which is why (unlike `panel.rs`'s real widget construction) it's fully
//! unit-testable: `mlua::Lua` itself needs no display.

use mlua::{Lua, RegistryKey, Table};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) struct CommandEntry {
    #[allow(dead_code)] // read by a future command-palette UI; not consumed by this plan
    pub(crate) title: String,
    pub(crate) keybinding: Option<String>,
    pub(crate) action: RegistryKey,
}

#[derive(Default)]
pub(crate) struct CommandRegistry {
    commands: HashMap<String, CommandEntry>,
}

impl CommandRegistry {
    pub(crate) fn register(&mut self, id: String, entry: CommandEntry) {
        if self.commands.insert(id.clone(), entry).is_some() {
            eprintln!("[lua] command '{id}' re-registered -- replaced");
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<&CommandEntry> {
        self.commands.get(id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &CommandEntry)> {
        self.commands.iter()
    }
}

pub(crate) fn install(
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
            .register(id, CommandEntry { title, keybinding, action: action_key });
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
}
