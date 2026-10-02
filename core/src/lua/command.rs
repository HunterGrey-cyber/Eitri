//! `eitri.command.register({ id, title, keybinding, action })`. `keybinding` is optional --
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
    // the action itself calls `eitri.command.register(...)`.
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

    /// How many commands `init.lua` has registered.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether none has been registered.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
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

/// Whether `id` is safe to embed in `main.rs`'s `cmd-{id}` GIO action name
/// (`set_accels_for_action("app.cmd-{id}", ...)`). A probe against the real libgtk-4
/// 4.22.5/glib2 2.88.3 this product links (sweep verdict "sw-lua-5",
/// `the private review notes`) found that once such an id is bound to a
/// keybinding, GTK does not return an error Rust code could catch: a space or `(`/`)` triggers a
/// GTK-CRITICAL then a hard assertion failure that core-dumps the whole process, and `::` parses
/// silently into a *different* action (`x::y` binds the accel to action `x`, target `y`) rather
/// than failing at all. So this has to be refused here, at registration, before `main.rs` ever
/// sees it: ASCII alphanumerics, `-` and `.` only. **Correction (codex-sweep round 1):** an
/// earlier revision of this comment called that "deliberately narrower than GIO's real
/// `g_action_name_is_valid`, which allows more punctuation" -- checked against the system
/// glib2 2.88.3 this product links (`Gio.Action.name_is_valid`), that is false: GIO's own rule
/// accepts exactly ASCII alphanumerics, `-` and `.` too (`a.b`/`a-b` valid; `a_b`, `a+b`, `a:b`,
/// `a/b`, non-ASCII and the empty string all rejected). The two rules are the same, not narrower
/// -- this is simply a from-scratch reimplementation so `core` need not depend on `gio`.
fn is_valid_command_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, registry: Rc<RefCell<CommandRegistry>>) -> mlua::Result<()> {
    let command_table = lua.create_table()?;
    let register_fn = lua.create_function(move |lua, spec: Table| {
        let id: String = spec.get("id")?;
        if !is_valid_command_id(&id) {
            // Reported through `LuaEngine::load_init_file`'s existing error path, not a GTK abort
            // at startup. **Correction (codex-sweep round 1):** an earlier revision of this
            // comment said "an uncaught Lua error there is logged and init.lua keeps loading" --
            // wrong. `load_init_file` runs the whole file as one `.exec()`'d chunk, so an
            // uncaught error here unwinds the rest of *that* chunk exactly like any other Lua
            // error: nothing registered after this call (later commands, keymap, panels,
            // `agent.account`, ...) runs (this file's own
            // `tests::an_invalid_id_stops_the_rest_of_the_chunk_not_just_that_call`). That round
            // also said the *shell* kept starting regardless, with whatever was registered before
            // the error; true then, and no longer:
            //
            // **The shell refuses to start** (whole-branch review): the Lua error alone
            // left one stderr line an app-menu launch never shows, skipped the rest of init.lua --
            // `agent.account` included -- and a `pcall` swallowed even that. The message is kept
            // (`CommandRegistry::refused`) whatever init.lua does with the error, and
            // `LuaEngine::load_init_file` exits naming it, as every other validated config value does.
            let message = format!(
                "eitri.command.register: invalid command id {id:?} -- only ASCII letters, digits, \
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
    eitri.set("command", command_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_a_command_and_the_action_is_callable() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            action_ran = false
            eitri.command.register({
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
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.command.register({ id = "no-key", title = "No Key", action = function() end })
            "#,
        )
        .exec()
        .unwrap();

        assert!(registry.borrow().get("no-key").unwrap().keybinding.is_none());
    }

    /// Regression test for the reentrancy panic the final review reproduced: a command action
    /// that itself calls `eitri.command.register(...)` -- registering another command from
    /// inside a command's own action -- must not panic with `BorrowMutError`. This mirrors
    /// `LuaEngine::invoke_command`'s own borrow/call sequence rather than going through
    /// `LuaEngine` directly, since this module (unlike `LuaEngine`) needs no display and is
    /// fully unit-testable on its own.
    #[test]
    fn action_that_registers_another_command_does_not_panic() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.command.register({
                id = "first",
                title = "First",
                action = function()
                    eitri.command.register({
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
        // action's own `eitri.command.register` call would panic with `BorrowMutError`.
        let action = {
            let commands = registry.borrow();
            let entry = commands.get("first").expect("command should be registered");
            entry.action.clone()
        };
        let f: mlua::Function = lua.registry_value(&action).unwrap();
        f.call::<()>(()).unwrap();

        assert!(registry.borrow().get("second").is_some());
    }

    /// Regression tests for sw-lua-5 (`the private review notes`): a
    /// probe against the real libgtk-4/glib2 this product links showed that `main.rs`'s
    /// `set_accels_for_action("app.cmd-{id}", ...)` hard-aborts the *whole process* with a GTK
    /// assertion failure when `id` contains a space or `(`/`)`, and silently mis-binds the accel
    /// to a different action when `id` contains `::` (`x::y` parses as action `x`, target `y`).
    /// `register` must refuse each of these before they ever reach `main.rs`, naming the bad id,
    /// rather than letting GTK crash the shell at startup.
    #[test]
    fn register_rejects_an_id_containing_a_space() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let err = lua
            .load(r#"eitri.command.register({ id = "my command", title = "x", action = function() end })"#)
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
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let err = lua
            .load(r#"eitri.command.register({ id = "foo(bar)", title = "x", action = function() end })"#)
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
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let err = lua
            .load(r#"eitri.command.register({ id = "x::y", title = "x", action = function() end })"#)
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
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.command.register({ id = "my-command.v2", title = "x", action = function() end })"#)
            .exec()
            .unwrap();
        assert!(registry.borrow().get("my-command.v2").is_some());
    }

    /// Regression test for a codex-sweep round-1 finding on this file's own comment: it claimed
    /// an uncaught error from an invalid id is "logged and init.lua keeps loading", implying
    /// later statements in the same file still run. A Lua chunk executed with `.exec()` (which
    /// is exactly how `shell::lua::LuaEngine::load_init_file` runs a whole `init.lua`) unwinds at
    /// its first uncaught error like any normal Lua script: nothing after the bad call in *that
    /// chunk* executes. This is standard Lua semantics, not anything `register` does specially,
    /// but the comment's wording was wrong about it, so this pins the real behaviour against
    /// `install`'s own `register_fn` directly, with no need for `LuaEngine`.
    #[test]
    fn an_invalid_id_stops_the_rest_of_the_chunk_not_just_that_call() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let result = lua
            .load(
                r#"
                before_the_bad_call = true
                eitri.command.register({ id = "bad id", title = "x", action = function() end })
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
    /// sw-lua-5, whole-branch review: the refusal was only a Lua error, so the rest of init.lua
    /// (the keymap, panels, the `agent.account` pin) was skipped and the shell started anyway with
    /// one stderr line an app-menu launch never shows -- and an init.lua that wrapped the call in
    /// `pcall` swallowed even that. A refused id is now remembered however the error is handled, so
    /// `LuaEngine::load_init_file` can make it a startup failure naming the id, as every other
    /// config value Eitri validates is (`agent.font_size`, `agent.account`, a keybinding).
    #[test]
    fn a_refused_id_is_remembered_even_when_init_lua_catches_the_error() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let registry = Rc::new(RefCell::new(CommandRegistry::default()));
        install(&lua, &eitri, registry.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        assert!(registry.borrow().refused().is_none());
        lua.load(
            r#"
            local ok = pcall(eitri.command.register, { id = "my command", title = "x", action = function() end })
            assert(not ok)
            eitri.command.register({ id = "fine", title = "y", action = function() end })
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
