//! `neovibe.config.get(key)` / `neovibe.config.set(key, value)`.
//!
//! v1 scope, deliberately minimal: in-memory only (nothing persists across a restart), and
//! values are strings only (no numbers/booleans/tables). The spec left config persistence and
//! typing as an implementation detail for this stage -- neither has a real use case yet driving
//! a more elaborate design, so this starts as small as it can and stays that way until a real
//! plugin actually needs more.

use mlua::{IntoLua, Lua, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `shell/src/lua/mod.rs::LuaEngine::new` constructs one directly
/// (`config::ConfigStore::default()`) to hand to `install`.
#[derive(Default)]
pub struct ConfigStore {
    values: HashMap<String, String>,
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(
    lua: &Lua,
    neovibe: &Table,
    store: Rc<RefCell<ConfigStore>>,
) -> mlua::Result<()> {
    let config_table = lua.create_table()?;

    let store_for_get = store.clone();
    let get_fn = lua.create_function(move |lua, key: String| -> mlua::Result<Value> {
        match store_for_get.borrow().values.get(&key) {
            Some(v) => v.clone().into_lua(lua),
            None => Ok(Value::Nil),
        }
    })?;

    let store_for_set = store.clone();
    let set_fn = lua.create_function(move |_, (key, value): (String, String)| {
        store_for_set.borrow_mut().values.insert(key, value);
        Ok(())
    })?;

    config_table.set("get", get_fn)?;
    config_table.set("set", set_fn)?;
    neovibe.set("config", config_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_roundtrips_through_real_lua() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &neovibe, store).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        lua.load(r#"neovibe.config.set("greeting", "hello")"#).exec().unwrap();
        let value: String = lua
            .load(r#"return neovibe.config.get("greeting")"#)
            .eval()
            .unwrap();
        assert_eq!(value, "hello");
    }

    #[test]
    fn get_of_unset_key_returns_nil() {
        let lua = Lua::new();
        let neovibe = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &neovibe, store).unwrap();
        lua.globals().set("neovibe", neovibe).unwrap();

        let is_nil: bool = lua
            .load(r#"return neovibe.config.get("never_set") == nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
    }
}
