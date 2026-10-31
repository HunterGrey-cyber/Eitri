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
    /// Shares startup validation with the other Lua APIs.
    init_refusals: Option<Rc<RefCell<super::kernel::InitRefusals>>>,
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

    /// Every key with the value [`Self::get`] gives for it, sorted by key.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .values
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().to_string()))
            .collect();
        entries.sort();
        entries
    }

    /// The first set refusal recorded at the Lua function boundary during init.
    /// Keeping it outside Lua makes a caught error visible to startup validation.
    pub fn refused(&self) -> Option<String> {
        self.init_refusals
            .as_ref()
            .and_then(|shared| shared.borrow().message("eitri.config.set"))
    }
}

/// The `init.lua` key: `eitri.config.set("window.restore", true | false)`. Whether the window that
/// holds a pair brings the second window back at start when it was open at the pair's last end.
pub const WINDOW_RESTORE_KEY: &str = "window.restore";

/// [`WINDOW_RESTORE_KEY`] as stored. Unset means the second window comes back: it is part of the
/// arrangement the user left, as the workspaces are.
pub fn parse_window_restore(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(WINDOW_RESTORE_KEY, value, true)
}

/// The editor's presentation preference, read after init.lua has run.
pub const EDITOR_PRESENTATION_KEY: &str = "editor.presentation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPresentation {
    Auto,
    Subsurface,
    Gtk,
}

pub fn parse_editor_presentation(value: Option<&str>) -> Result<EditorPresentation, String> {
    match value {
        None | Some("auto") => Ok(EditorPresentation::Auto),
        Some("subsurface") => Ok(EditorPresentation::Subsurface),
        Some("gtk") => Ok(EditorPresentation::Gtk),
        Some(value) => Err(format!(
            "{EDITOR_PRESENTATION_KEY:?} must be auto, subsurface or gtk (got {value:?})"
        )),
    }
}

/// The terminal's optional family and height overrides, read after init.lua has run.
pub const TERMINAL_FONT_KEY: &str = "terminal.font";

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFontSetting {
    pub families: Vec<String>,
    pub size_px: Option<f32>,
}

/// An omitted part follows the editor; an invalid explicit height refuses startup rather than
/// silently substituting the editor's default.
pub fn parse_terminal_font(value: Option<&str>) -> Result<Option<TerminalFontSetting>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let invalid =
        || format!("{TERMINAL_FONT_KEY:?} must name font families or a height in (0, 100] points (got {value:?})");
    let families = crate::theme::guifont_families(value);
    let height = value
        .split(':')
        .skip(1)
        .filter_map(|part| part.strip_prefix('h'))
        .last();
    let size_px = match height {
        Some(height) => {
            let points = height.parse::<f32>().map_err(|_| invalid())?;
            if !points.is_finite() || points <= 0.0 || points > 100.0 {
                return Err(invalid());
            }
            Some(if families.is_empty() {
                // A placeholder family lets a height-only override use the editor's pixel conversion.
                crate::theme::guifont_size_px(&format!("monospace:h{height}"))
            } else {
                crate::theme::guifont_size_px(value)
            })
        }
        None => None,
    };
    if families.is_empty() && size_px.is_none() {
        return Err(invalid());
    }
    Ok(Some(TerminalFontSetting { families, size_px }))
}

/// `pub`: `Kernel::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<ConfigStore>>) -> mlua::Result<()> {
    store.borrow_mut().init_refusals = Some(super::kernel::init_refusals(lua));
    let config_table = lua.create_table()?;

    let store_for_get = store.clone();
    let get_fn = super::kernel::registering_function(
        lua,
        "eitri.config.get",
        move |lua, key: String| -> mlua::Result<Value> {
            match store_for_get.borrow().values.get(&key) {
                Some(Stored::Text(text)) => text.clone().into_lua(lua),
                Some(Stored::Switch(on)) => Ok(Value::Boolean(*on)),
                None => Ok(Value::Nil),
            }
        },
    )?;

    let store_for_set = store.clone();
    let set_fn =
        super::kernel::registering_function(lua, "eitri.config.set", move |lua, (key, value): (Value, Value)| {
            let key = match key {
                Value::String(ref text) => text.to_str().map(|text| text.to_string()).ok(),
                _ => None,
            };
            let Some(key) = key else {
                return Err(mlua::Error::RuntimeError(
                    "eitri.config.set: the key must be a string".to_string(),
                ));
            };
            let stored = match value {
                // `nil` unsets the key, as assigning `nil` removes a Lua table's entry. It is also what
                // `os.getenv` gives for an unset variable.
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
                Err(why) => Err(mlua::Error::RuntimeError(format!(
                    "eitri.config.set({key:?}, ...): {why}"
                ))),
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
    fn entries_are_sorted_and_read_as_get_reads() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        assert!(store.borrow().entries().is_empty());
        lua.load(
            "eitri.config.set('z.text', 'word')\n\
             eitri.config.set('a.number', 200)\n\
             eitri.config.set('m.switch', false)\n\
             eitri.config.set('gone', 'x')\n\
             eitri.config.set('gone', nil)\n",
        )
        .exec()
        .unwrap();
        let entries = store.borrow().entries();
        let entries: Vec<(&str, &str)> = entries.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            entries,
            [("a.number", "200"), ("m.switch", "false"), ("z.text", "word")]
        );
    }

    #[test]
    fn terminal_font_parses_optional_families_and_height() {
        assert_eq!(TERMINAL_FONT_KEY, "terminal.font");
        assert_eq!(parse_terminal_font(None), Ok(None));
        for (value, families, size_px) in [
            (
                "Iosevka:h12",
                vec!["Iosevka"],
                Some(crate::theme::guifont_size_px("Iosevka:h12")),
            ),
            ("Iosevka", vec!["Iosevka"], None),
            (":h12", vec![], Some(crate::theme::guifont_size_px("Iosevka:h12"))),
            (
                "Fira\\ Code,Symbols_Nerd_Font:h10:h13.5:b",
                vec!["Fira Code", "Symbols Nerd Font"],
                Some(crate::theme::guifont_size_px("Mono:h13.5")),
            ),
            (":h100", vec![], Some(crate::theme::guifont_size_px("Mono:h100"))),
            (
                "Mono:hx:h12",
                vec!["Mono"],
                Some(crate::theme::guifont_size_px("Mono:h12")),
            ),
        ] {
            assert_eq!(
                parse_terminal_font(Some(value)),
                Ok(Some(TerminalFontSetting {
                    families: families.into_iter().map(str::to_owned).collect(),
                    size_px,
                })),
                "{value:?}"
            );
        }
    }

    #[test]
    fn terminal_font_refuses_empty_settings_and_invalid_heights_naming_the_key_and_value() {
        for value in [
            "",
            "  , ",
            ":b",
            ":h0",
            ":hx",
            ":h101",
            ":h-1",
            ":hNaN",
            ":hinf",
            ":h",
            "Mono:h12:hx",
            "Mono:h12:h",
            "Mono:h101",
        ] {
            let error = parse_terminal_font(Some(value)).unwrap_err();
            assert!(error.contains(TERMINAL_FONT_KEY), "{error}");
            assert!(error.contains(&format!("{value:?}")), "{error}");
        }
    }

    #[test]
    fn editor_presentation_parses_three_words_and_refuses_others() {
        assert_eq!(parse_editor_presentation(None), Ok(EditorPresentation::Auto));
        for (word, expected) in [
            ("auto", EditorPresentation::Auto),
            ("subsurface", EditorPresentation::Subsurface),
            ("gtk", EditorPresentation::Gtk),
        ] {
            assert_eq!(parse_editor_presentation(Some(word)), Ok(expected));
        }
        for bad in ["x", "", "Auto", " gtk", "subsurface\n", "true"] {
            let error = parse_editor_presentation(Some(bad)).unwrap_err();
            for word in ["editor.presentation", "auto", "subsurface", "gtk"] {
                assert!(error.contains(word), "{error}");
            }
        }
    }

    #[test]
    fn window_restore_takes_a_boolean_and_names_itself_otherwise() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        let read = || parse_window_restore(store.borrow().get(WINDOW_RESTORE_KEY));

        assert_eq!(read(), Ok(true), "unset, the second window comes back");
        for (set, want) in [("false", false), ("true", true), ("'off'", false), ("'on'", true)] {
            lua.load(format!("eitri.config.set('window.restore', {set})"))
                .exec()
                .unwrap();
            assert_eq!(read(), Ok(want), "{set}");
        }
        for bad in ["'later'", "1"] {
            lua.load(format!("eitri.config.set('window.restore', {bad})"))
                .exec()
                .unwrap();
            let err = read().unwrap_err();
            assert!(err.contains("\"window.restore\""), "the refusal names its key: {err}");
            assert!(err.contains("must be true or \"on\" / false or \"off\""), "{err}");
        }
    }

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
            store.borrow().refused().as_deref(),
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
