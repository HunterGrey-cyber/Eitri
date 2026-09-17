//! `neovibe.on(event_name, handler)` -- subscribe a Lua function to a named event. v1 ships
//! exactly one real event, `"shell:ready"` (fired once, after `init.lua` has loaded and the
//! window is presented) -- more get added incrementally as real plugins need them, matching
//! this project's stated "small set of concrete extension points, expanded as needed" approach
//! rather than speculatively designing a full event taxonomy now.

use mlua::{Lua, RegistryKey, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `shell/src/lua/mod.rs::LuaEngine::new` constructs one directly
/// (`event::EventBus::default()`) to hand to `install`.
#[derive(Default)]
pub struct EventBus {
    // `Rc<RegistryKey>`, not a bare `RegistryKey`: `RegistryKey` itself doesn't implement
    // `Clone` (verified against the installed mlua 0.12.1's `src/types/registry_key.rs`), and
    // `emit` below needs to clone handles *out* of a borrowed `EventBus` before dropping that
    // borrow -- see `emit`'s own comment for why.
    handlers: HashMap<String, Vec<Rc<RegistryKey>>>,
}

impl EventBus {
    /// Private: only `install`'s own closure below calls this. Nothing in `shell` (or anywhere
    /// outside this file) calls `subscribe` directly.
    fn subscribe(&mut self, event_name: String, handler: RegistryKey) {
        self.handlers.entry(event_name).or_default().push(Rc::new(handler));
    }
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(lua: &Lua, neovibe: &Table, bus: Rc<RefCell<EventBus>>) -> mlua::Result<()> {
    let on_fn = lua.create_function(move |lua, (event_name, handler): (String, mlua::Function)| {
        let key = lua.create_registry_value(handler)?;
        bus.borrow_mut().subscribe(event_name, key);
        Ok(())
    })?;
    neovibe.set("on", on_fn)?;
    Ok(())
}

/// Calls every handler subscribed to `event_name`, in registration order, with `payload`. A
/// handler that errors is logged and skipped -- one broken plugin handler must not stop the
/// rest, matching `LuaEngine::load_init_file`'s "never let Lua crash the shell" discipline
/// (added in Task 6).
///
/// Takes `bus: &RefCell<EventBus>` (not an already-borrowed `&EventBus`) so it can control its
/// own borrow's lifetime: the borrow is taken only long enough to clone out the `Rc<RegistryKey>`
/// handles for `event_name`, then dropped *before* any handler function is actually called. This
/// is required, not just tidy -- a handler that itself calls `neovibe.on(...)` (subscribing
/// another handler, possibly to this same event) needs `EventBus::subscribe`'s
/// `bus.borrow_mut()` to succeed, which would panic with `BorrowMutError` if this function's own
/// borrow of `bus` were still held while the handler ran. Same reentrancy hazard, and the same
/// fix (resolve what's needed under the borrow, drop the borrow, then call out), as
/// `neovide-editor`'s tick callback uses for its `exited_callback`.
///
/// `pub`: `LuaEngine::emit` calls this.
pub fn emit(lua: &Lua, bus: &RefCell<EventBus>, event_name: &str, payload: Value) {
    let keys: Vec<Rc<RegistryKey>> = {
        let bus = bus.borrow();
        match bus.handlers.get(event_name) {
            Some(handlers) => handlers.clone(),
            None => return,
        }
    };
    for key in &keys {
        let Ok(f) = lua.registry_value::<mlua::Function>(key) else { continue };
        if let Err(err) = f.call::<()>(payload.clone()) {
            eprintln!("[lua] event '{event_name}' handler error: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribed_handler_runs_on_emit() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let bus = Rc::new(RefCell::new(EventBus::default()));
        install(&lua, &neovibe, bus.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(
            r#"
            handler_ran = false
            neovibe.on("test_event", function() handler_ran = true end)
            "#,
        )
        .exec()
        .unwrap();

        emit(&lua, &bus, "test_event", Value::Nil);

        let ran: bool = lua.globals().get("handler_ran").unwrap();
        assert!(ran);
    }

    #[test]
    fn emit_of_unsubscribed_event_is_a_harmless_no_op() {
        let lua = Lua::new();
        let bus = RefCell::new(EventBus::default());
        // Must not panic even though nothing ever subscribed to "nothing_here".
        emit(&lua, &bus, "nothing_here", Value::Nil);
    }

    /// Regression test for the reentrancy panic the final review reproduced: a handler for
    /// `"test_event"` that itself calls `neovibe.on("test_event", ...)` -- subscribing another
    /// handler to the very event currently being emitted -- must not panic with `BorrowMutError`.
    /// Before the fix, `emit` held `bus.borrow()` for the entire loop (including while each
    /// handler ran), so `EventBus::subscribe`'s `bus.borrow_mut()` from inside the handler would
    /// panic; the fix clones the handler list out and drops the borrow before calling any of
    /// them.
    #[test]
    fn handler_that_subscribes_another_handler_to_the_same_event_does_not_panic() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let bus = Rc::new(RefCell::new(EventBus::default()));
        install(&lua, &neovibe, bus.clone()).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(
            r#"
            second_handler_ran = false
            neovibe.on("test_event", function()
                neovibe.on("test_event", function()
                    second_handler_ran = true
                end)
            end)
            "#,
        )
        .exec()
        .unwrap();

        // Must not panic.
        emit(&lua, &bus, "test_event", Value::Nil);

        // The newly-subscribed handler wasn't called during this same `emit` (it wasn't in the
        // list `emit` snapshotted at the start) -- a second `emit` picks it up.
        let ran_after_first: bool = lua.globals().get("second_handler_ran").unwrap();
        assert!(!ran_after_first);

        emit(&lua, &bus, "test_event", Value::Nil);
        let ran_after_second: bool = lua.globals().get("second_handler_ran").unwrap();
        assert!(ran_after_second);
    }
}
