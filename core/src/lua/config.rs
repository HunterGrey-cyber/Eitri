//! `eitri.config.get(key)` / `eitri.config.set(key, value)`.
//!
//! v1 scope, deliberately minimal: in-memory only (nothing persists across a restart), and
//! values are strings or booleans (no tables; `nil` unsets a key; anything else is refused, see
//! `ConfigStore::refused`). A number is stored as its text, as Lua's own string
//! coercion gives it. A boolean stays a boolean, so a switch can be written
//! `eitri.config.set("key", false)` and `eitri.config.get("key")` gives `false` back: storing the
//! word instead would hand Lua the string `"false"`, which Lua counts as true. Rust reads a
//! boolean as the word `"true"` or `"false"`.

use mlua::{FromLua, IntoLua, Lua, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `Kernel::new` constructs one directly
/// (`config::ConfigStore::default()`) to hand to `install`.
#[derive(Default)]
pub struct ConfigStore {
    values: HashMap<String, Stored>,
    /// The first `set` this store could not hold, as the startup-failure text.
    refused: Option<String>,
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

    /// The message of the first `eitri.config.set` that could not be stored, or `None`. A value
    /// that is not a string, a number, a boolean or `nil` (a table, a function), a string that is
    /// not UTF-8, or a key that is not a string never reaches the store, so every key Eitri reads
    /// would silently keep its default -- `agent.account` among them, which then spends whichever
    /// account launched the window. `Kernel::load_init_file` makes it a startup failure once
    /// init.lua has run; it is remembered here because a `pcall` around the call swallows the Lua
    /// error.
    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }
}

/// Remembers the first refusal for `ConfigStore::refused` and hands back the Lua error, so the rest
/// of the chunk stops as it does for any other error.
fn refuse(store: &RefCell<ConfigStore>, message: String) -> mlua::Error {
    store.borrow_mut().refused.get_or_insert_with(|| message.clone());
    mlua::Error::RuntimeError(message)
}

/// `pub`: `Kernel::new` calls this.
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
    let set_fn = lua.create_function(move |lua, (key, value): (Value, Value)| {
        let key = match key {
            Value::String(ref text) => text.to_str().map(|text| text.to_string()).ok(),
            _ => None,
        };
        let Some(key) = key else {
            return Err(refuse(
                &store_for_set,
                "eitri.config.set: the key must be a string".to_string(),
            ));
        };
        let stored = match value {
            // `nil` unsets the key, as assigning `nil` removes a Lua table's entry. It is also what
            // `os.getenv` gives for an unset variable, and an empty string already means unset to
            // every key Eitri reads.
            Value::Nil => {
                store_for_set.borrow_mut().values.remove(&key);
                return Ok(());
            }
            Value::Boolean(on) => Ok(Stored::Switch(on)),
            Value::String(_) | Value::Integer(_) | Value::Number(_) => String::from_lua(value, lua)
                .map(Stored::Text)
                .map_err(|_| "the string is not UTF-8".to_string()),
            other => Err(format!(
                "a {} value cannot be stored (only a string, a number or a boolean)",
                other.type_name()
            )),
        };
        match stored {
            Ok(stored) => {
                store_for_set.borrow_mut().values.insert(key, stored);
                Ok(())
            }
            Err(why) => Err(refuse(&store_for_set, format!("eitri.config.set({key:?}, ...): {why}"))),
        }
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

    /// A value the store cannot hold is refused by key, and remembered even when init.lua catches
    /// the error, so the shell can refuse to start instead of running on the key's default.
    #[test]
    fn a_value_the_store_cannot_hold_is_refused_and_remembered_through_a_pcall() {
        for (value, expected) in [
            (
                "{}",
                "eitri.config.set(\"agent.account\", ...): a table value cannot be stored",
            ),
            (
                "function() end",
                "eitri.config.set(\"agent.account\", ...): a function value cannot be stored",
            ),
            (
                "\"\\255\"",
                "eitri.config.set(\"agent.account\", ...): the string is not UTF-8",
            ),
        ] {
            let lua = Lua::new();
            let eitri = lua.create_table().unwrap();
            let store = Rc::new(RefCell::new(ConfigStore::default()));
            install(&lua, &eitri, store.clone()).unwrap();
            lua.globals().set("eitri", eitri).unwrap();

            lua.load(format!(
                r#"
                local ok = pcall(eitri.config.set, "agent.account", {value})
                assert(not ok)
                eitri.config.set("agent.font_size", 14)
                "#
            ))
            .exec()
            .expect("the chunk caught the refusal and went on");
            let store = store.borrow();
            let refused = store.refused().unwrap_or_else(|| panic!("{value} was not refused"));
            assert!(refused.starts_with(expected), "{value}: {refused}");
            assert_eq!(store.get("agent.account"), None, "{value} must not be stored");
            assert_eq!(store.get("agent.font_size"), Some("14"));
        }
    }

    /// `nil` unsets, the way `os.getenv` reports an unset variable: what was set before is gone and
    /// nothing is refused.
    #[test]
    fn nil_unsets_a_key_rather_than_being_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.config.set("agent.account", "work")
            eitri.config.set("agent.account", nil)
            eitri.config.set("agent.never", os.getenv("EITRI_TEST_SURELY_UNSET_VARIABLE"))
            "#,
        )
        .exec()
        .unwrap();
        let store = store.borrow();
        assert_eq!(store.get("agent.account"), None);
        assert_eq!(store.get("agent.never"), None);
        assert_eq!(store.refused(), None);
    }

    #[test]
    fn a_key_that_is_not_a_string_is_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        assert!(lua.load(r#"eitri.config.set({}, "work")"#).exec().is_err());
        assert_eq!(
            store.borrow().refused(),
            Some("eitri.config.set: the key must be a string")
        );
    }

    #[test]
    fn strings_numbers_and_booleans_are_never_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("a", "x"); eitri.config.set("b", 1.5); eitri.config.set("c", true)"#)
            .exec()
            .unwrap();
        assert_eq!(store.borrow().refused(), None);
    }
}
