//! `eitri.config.get(key)` / `eitri.config.set(key, value)`.
//!
//! v1 scope, deliberately minimal: in-memory only (nothing persists across a restart), and
//! values are strings or booleans (no tables). A number is stored as its text, as Lua's own string
//! coercion gives it. A boolean stays a boolean, so a switch can be written
//! `eitri.config.set("key", false)` and `eitri.config.get("key")` gives `false` back: storing the
//! word instead would hand Lua the string `"false"`, which Lua counts as true. Rust reads a
//! boolean as the word `"true"` or `"false"`.

use mlua::{FromLua, IntoLua, Lua, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `shell/src/lua/mod.rs::LuaEngine::new` constructs one directly
/// (`config::ConfigStore::default()`) to hand to `install`.
#[derive(Default)]
pub struct ConfigStore {
    values: HashMap<String, Stored>,
}

/// One stored value: the text of a string or number, or a boolean kept as one.
enum Stored {
    Text(String),
    Switch(bool),
}

impl Stored {
    fn as_str(&self) -> &str {
        match self {
            Stored::Text(text) => text,
            Stored::Switch(true) => "true",
            Stored::Switch(false) => "false",
        }
    }
}

impl ConfigStore {
    /// Reads a key from Rust. Added 2026-09-21 for `agent.account`: `init.lua` is where the host
    /// is configured, and the host has to be able to read what it was configured with. This is the
    /// same store `eitri.config.get` reads, not a parallel one -- a plugin and the shell see one
    /// value for one key, which is the only arrangement that cannot drift.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(Stored::as_str)
    }
}

/// `pub`: `LuaEngine::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<ConfigStore>>) -> mlua::Result<()> {
    let config_table = lua.create_table()?;

    let store_for_get = store.clone();
    let get_fn = lua.create_function(move |lua, key: String| -> mlua::Result<Value> {
        match store_for_get.borrow().values.get(&key) {
            Some(Stored::Text(text)) => text.clone().into_lua(lua),
            Some(Stored::Switch(on)) => Ok(Value::Boolean(*on)),
            None => Ok(Value::Nil),
        }
    })?;

    let store_for_set = store.clone();
    let set_fn = lua.create_function(move |lua, (key, value): (String, Value)| {
        let stored = match value {
            Value::Boolean(on) => Stored::Switch(on),
            other => Stored::Text(String::from_lua(other, lua)?),
        };
        store_for_set.borrow_mut().values.insert(key, stored);
        Ok(())
    })?;

    config_table.set("get", get_fn)?;
    config_table.set("set", set_fn)?;
    eitri.set("config", config_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_roundtrips_through_real_lua() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("greeting", "hello")"#).exec().unwrap();
        let value: String = lua.load(r#"return eitri.config.get("greeting")"#).eval().unwrap();
        assert_eq!(value, "hello");
    }

    #[test]
    fn what_init_lua_set_is_readable_from_rust_through_the_same_store() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("agent.account", "work")"#)
            .exec()
            .unwrap();
        assert_eq!(store.borrow().get("agent.account"), Some("work"));
        assert_eq!(store.borrow().get("agent.nothing"), None);
    }

    #[test]
    fn a_boolean_is_stored_as_its_word_and_a_number_as_its_text() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.config.set("a.on", true)
            eitri.config.set("a.off", false)
            eitri.config.set("a.size", 12)
            eitri.config.set("a.word", "false")
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(store.borrow().get("a.on"), Some("true"));
        assert_eq!(store.borrow().get("a.off"), Some("false"));
        assert_eq!(store.borrow().get("a.size"), Some("12"));
        assert_eq!(store.borrow().get("a.word"), Some("false"));
    }

    #[test]
    fn a_boolean_comes_back_to_lua_as_a_boolean() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let (off, on, word): (bool, bool, String) = lua
            .load(
                r#"
                eitri.config.set("a.off", false)
                eitri.config.set("a.on", true)
                eitri.config.set("a.word", "false")
                return eitri.config.get("a.off") == false, eitri.config.get("a.on") == true,
                    eitri.config.get("a.word")
                "#,
            )
            .eval()
            .unwrap();
        assert!(off && on);
        assert_eq!(word, "false");
    }

    #[test]
    fn a_table_is_still_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        assert!(lua.load(r#"eitri.config.set("a.t", {})"#).exec().is_err());
    }

    #[test]
    fn get_of_unset_key_returns_nil() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let is_nil: bool = lua
            .load(r#"return eitri.config.get("never_set") == nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
    }
}
