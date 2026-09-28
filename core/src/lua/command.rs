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
    /// The first refusal `register` raised (sw-lua-5), kept however init.lua handled the Lua error
    /// -- a `pcall` included -- so the shell can refuse to start on it (`refused`).
    refused: Option<String>,
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

    /// The message of the first command id `register` refused, or `None`. `pub`:
    /// `LuaEngine::load_init_file` makes it a startup failure once init.lua has run.
    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    /// `pub`: `main.rs` iterates every registered command to wire a real GTK action for it.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &CommandEntry)> {
        self.commands.iter()
    }
}

/// Whether `id` is safe in `main.rs`'s `cmd-{id}` GIO action name and
/// `set_accels_for_action("app.cmd-{id}", ...)`. Spaces or parentheses in a bound id can trigger
/// a GTK assertion that aborts the process. `::` can silently bind a different action:
/// `x::y` parses as action `x` with target `y`.
/// Validate at registration before GTK sees the id. Like GIO's `g_action_name_is_valid`,
/// accept only a nonempty sequence of ASCII alphanumerics, `-` and `.`.
/// This implementation keeps `core` independent of `gio`.
fn is_valid_command_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(lua: &Lua, neovibe: &Table, registry: Rc<RefCell<CommandRegistry>>) -> mlua::Result<()> {
    let command_table = lua.create_table()?;
    let register_fn = lua.create_function(move |lua, spec: Table| {
        let id: String = spec.get("id")?;
        if !is_valid_command_id(&id) {
            // An uncaught error here stops the rest of the `.exec()` chunk, including later commands,
            // keymaps, panels and `agent.account`. A `pcall` can swallow that Lua error, so also retain the
            // message in `CommandRegistry::refused`. `load_init_file` checks it and refuses startup with
            // the message even if Lua caught the error. This keeps invalid ids from reaching GTK.
            let message = format!(
                "neovibe.command.register: invalid command id {id:?} -- only ASCII letters, digits, \
                 '-' and '.' are allowed (an invalid id crashes the whole process once bound to a keybinding)"
            );
            registry.borrow_mut().refused.get_or_insert_with(|| message.clone());
            return Err(mlua::Error::RuntimeError(message));
        }
        let title: String = spec.get("title")?;
        let keybinding: Option<String> = spec.get("keybinding")?;
        let action: mlua::Function = spec.get("action")?;
        let action_key = lua.create_registry_value(action)?;
        registry.borrow_mut().register(
            id,
            CommandEntry {
                title,
                keybinding,
                action: Rc::new(action_key),
            },
        );
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

    /// Registration refuses ids containing spaces, parentheses or `::` before they reach
    /// `set_accels_for_action("app.cmd-{id}", ...)`. Spaces and parentheses can abort the process
    /// with a GTK assertion; `x::y` silently targets action `x` with target `y`.
    /// The error must name the invalid id.
    #[test]
    fn register_rejects_an_id_containing_a_space() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        let err = lua
            .load(r#"neovibe.command.register({ id = "my command", title = "x", action = function() end })"#)
            .exec()
            .expect_err("an id containing a space must be refused, not crash GTK once bound");
        assert!(
            err.to_string().contains("my command"),
            "error should name the bad id: {err}"
        );
        assert!(registry.borrow().get("my command").is_none());
    }

    #[test]
    fn register_rejects_an_id_containing_parentheses() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        let err = lua
            .load(r#"neovibe.command.register({ id = "foo(bar)", title = "x", action = function() end })"#)
            .exec()
            .expect_err("an id containing parentheses must be refused, not crash GTK once bound");
        assert!(
            err.to_string().contains("foo(bar)"),
            "error should name the bad id: {err}"
        );
        assert!(registry.borrow().get("foo(bar)").is_none());
    }

    #[test]
    fn register_rejects_an_id_containing_a_double_colon() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        let err = lua
            .load(r#"neovibe.command.register({ id = "x::y", title = "x", action = function() end })"#)
            .exec()
            .expect_err(
                "an id containing '::' must be refused -- it silently mis-binds the accel to a different action",
            );
        assert!(err.to_string().contains("x::y"), "error should name the bad id: {err}");
        assert!(registry.borrow().get("x::y").is_none());
    }

    #[test]
    fn register_accepts_alphanumerics_hyphen_and_dot() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(r#"neovibe.command.register({ id = "my-command.v2", title = "x", action = function() end })"#)
            .exec()
            .unwrap();
        assert!(registry.borrow().get("my-command.v2").is_some());
    }

    /// A Lua chunk run with `.exec()` stops at its first uncaught error. An invalid command id
    /// therefore prevents all later statements in that chunk from running. Exercise the
    /// registration closure from `install` directly to check this without starting the shell.
    #[test]
    fn an_invalid_id_stops_the_rest_of_the_chunk_not_just_that_call() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        let result = lua
            .load(
                r#"
                before_the_bad_call = true
                neovibe.command.register({ id = "bad id", title = "x", action = function() end })
                after_the_bad_call = true
                "#,
            )
            .exec();
        assert!(result.is_err(), "the invalid id must still be refused");

        let before: bool = lua.globals().get("before_the_bad_call").unwrap();
        assert!(before, "the statement before the bad call should have run");
        let after: mlua::Value = lua.globals().get("after_the_bad_call").unwrap();
        assert!(
            after.is_nil(),
            "the statement after the bad call must NOT have run -- the chunk stops at the error, \
             it does not keep loading"
        );
    }
    /// A refused command id is remembered even if `init.lua` catches the Lua error with `pcall`.
    /// `LuaEngine::load_init_file` can then fail startup and name the id. A Lua error alone can skip
    /// the rest of `init.lua` while allowing startup, with only stderr output that an app-menu launch
    /// may not show.
    #[test]
    fn a_refused_id_is_remembered_even_when_init_lua_catches_the_error() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &neovibe, registry.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        assert!(registry.borrow().refused().is_none());
        lua.load(
            r#"
            local ok = pcall(neovibe.command.register, { id = "my command", title = "x", action = function() end })
            assert(not ok)
            neovibe.command.register({ id = "fine", title = "y", action = function() end })
            "#,
        )
        .exec()
        .expect("the chunk itself caught the refusal and went on");
        let refused = registry
            .borrow()
            .refused()
            .map(str::to_owned)
            .expect("the refusal was remembered");
        assert!(refused.contains("my command"), "names the id: {refused}");
        assert!(registry.borrow().get("fine").is_some());
    }
}
