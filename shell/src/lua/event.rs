//! `neovibe.on(event_name, handler)` -- subscribe a Lua function to a named event. v1 ships
//! exactly one real event, `"shell:ready"` (fired once, after `init.lua` has loaded and the
//! window is presented) -- more get added incrementally as real plugins need them, matching
//! this project's stated "small set of concrete extension points, expanded as needed" approach
//! rather than speculatively designing a full event taxonomy now.

use mlua::{Lua, RegistryKey, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Default)]
pub(crate) struct EventBus {
    handlers: HashMap<String, Vec<RegistryKey>>,
}

impl EventBus {
    fn subscribe(&mut self, event_name: String, handler: RegistryKey) {
        self.handlers.entry(event_name).or_default().push(handler);
    }
}

pub(crate) fn install(lua: &Lua, neovibe: &Table, bus: Rc<RefCell<EventBus>>) -> mlua::Result<()> {
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
///.
pub(crate) fn emit(lua: &Lua, bus: &EventBus, event_name: &str, payload: Value) {
    let Some(handlers) = bus.handlers.get(event_name) else { return };
    for key in handlers {
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

        emit(&lua, &bus.borrow(), "test_event", Value::Nil);

        let ran: bool = lua.globals().get("handler_ran").unwrap();
        assert!(ran);
    }

    #[test]
    fn emit_of_unsubscribed_event_is_a_harmless_no_op() {
        let lua = Lua::new();
        let bus = EventBus::default();
        // Must not panic even though nothing ever subscribed to "nothing_here".
        emit(&lua, &bus, "nothing_here", Value::Nil);
    }
}
