//! `eitri.keymap.prefix(key)`, `.set(table, key, action, opts)`, `.del(table, key)` (keymap spec
//! §2.3). Each call is **recorded, never applied or checked here**: `shell` hands the recording to
//! `eitri_core::keymap::Keymap::apply_user` once `init.lua` has run, so every mistake -- a bad
//! key, an unknown action, a collision -- is a startup failure naming the call. A Lua error raised
//! from here would only be logged (`LuaEngine::load_init_file`), and the window would open with a
//! keymap the user did not write. That includes an argument of the wrong type: it is recorded as
//! [`KeymapOp::Invalid`].

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{FromLua, Lua, Table, Value};

use crate::keymap::{KeymapOp, OptValue};

/// The calls `init.lua` made, in order.
#[derive(Default)]
pub struct KeymapStore {
    ops: Vec<KeymapOp>,
}

impl KeymapStore {
    pub fn ops(&self) -> &[KeymapOp] {
        &self.ops
    }
}

fn text(lua: &Lua, value: &Value) -> Option<String> {
    match value {
        Value::String(_) => String::from_lua(value.clone(), lua).ok(),
        _ => None,
    }
}

fn opts(lua: &Lua, value: &Value) -> Result<Vec<(String, OptValue)>, String> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => return Err(format!("the options must be a table, not a {}", other.type_name())),
    };
    let mut out = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|e| e.to_string())?;
        let name = text(lua, &name).ok_or_else(|| "every option has a name, as { cells = 5 }".to_string())?;
        let value = match value {
            Value::Integer(i) => OptValue::Int(i),
            Value::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => OptValue::Int(n as i64),
            Value::Boolean(b) => OptValue::Bool(b),
            Value::String(_) => OptValue::Str(text(lua, &value).unwrap_or_default()),
            other => OptValue::Other(other.type_name()),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// `pub`: `shell::lua::LuaEngine::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<KeymapStore>>) -> mlua::Result<()> {
    let keymap = lua.create_table()?;

    let s = store.clone();
    keymap.set(
        "prefix",
        lua.create_function(move |lua, key: Value| {
            let op = match text(lua, &key) {
                Some(key) => KeymapOp::Prefix { key },
                None => KeymapOp::Invalid(format!(
                    "eitri.keymap.prefix(…): the key must be a string, not a {}",
                    key.type_name()
                )),
            };
            s.borrow_mut().ops.push(op);
            Ok(())
        })?,
    )?;

    let s = store.clone();
    keymap.set(
        "set",
        lua.create_function(
            move |lua, (table, key, action, options): (Value, Value, Value, Value)| {
                let op = match (text(lua, &table), text(lua, &key), text(lua, &action)) {
                    (Some(table), Some(key), Some(action)) => match opts(lua, &options) {
                        Ok(opts) => KeymapOp::Set {
                            table,
                            key,
                            action,
                            opts,
                        },
                        Err(why) => {
                            KeymapOp::Invalid(format!("eitri.keymap.set({table:?}, {key:?}, {action:?}, …): {why}"))
                        }
                    },
                    _ => KeymapOp::Invalid(
                        "eitri.keymap.set(table, key, action, opts): table, key and action must be strings".to_string(),
                    ),
                };
                s.borrow_mut().ops.push(op);
                Ok(())
            },
        )?,
    )?;

    let s = store;
    keymap.set(
        "del",
        lua.create_function(move |lua, (table, key): (Value, Value)| {
            let op = match (text(lua, &table), text(lua, &key)) {
                (Some(table), Some(key)) => KeymapOp::Del { table, key },
                _ => KeymapOp::Invalid("eitri.keymap.del(table, key): table and key must be strings".to_string()),
            };
            s.borrow_mut().ops.push(op);
            Ok(())
        })?,
    )?;

    eitri.set("keymap", keymap)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Action, KeySpec, Keymap, SwapTarget, TabAction};
    use crate::layout::{Axis, Direction};

    fn run(source: &str) -> Vec<KeymapOp> {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(KeymapStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        lua.load(source).exec().unwrap();
        let ops = store.borrow().ops().to_vec();
        ops
    }

    const SNIPPET: &str = include_str!("../../../docs/keymap/tmux-ctrl-a.lua");

    #[test]
    fn calls_are_recorded_in_order_with_their_options() {
        let ops = run(r#"
            eitri.keymap.prefix("C-a")
            eitri.keymap.del("prefix", "C-b")
            eitri.keymap.set("prefix", "h", "resize.left", { cells = 5, repeatable = true })
        "#);
        assert_eq!(ops[0], KeymapOp::Prefix { key: "C-a".into() });
        assert_eq!(
            ops[1],
            KeymapOp::Del {
                table: "prefix".into(),
                key: "C-b".into()
            }
        );
        let KeymapOp::Set {
            table,
            key,
            action,
            opts,
        } = &ops[2]
        else {
            panic!("{:?}", ops[2])
        };
        assert_eq!(
            (table.as_str(), key.as_str(), action.as_str()),
            ("prefix", "h", "resize.left")
        );
        let mut opts = opts.clone();
        opts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            opts,
            vec![
                ("cells".into(), OptValue::Int(5)),
                ("repeatable".into(), OptValue::Bool(true))
            ]
        );
    }

    /// A wrong argument type is not a Lua error (which `shell` only logs): it is recorded, and
    /// becomes the startup failure.
    #[test]
    fn a_call_with_a_wrong_argument_is_recorded_as_invalid_naming_the_call() {
        for source in [
            "eitri.keymap.prefix(3)",
            "eitri.keymap.set('prefix', 'g')",
            "eitri.keymap.set('prefix', 'g', 'zoom', 'fast')",
            "eitri.keymap.set('prefix', 'g', 'zoom', { [1] = true })",
            "eitri.keymap.del('prefix')",
        ] {
            let ops = run(source);
            let [KeymapOp::Invalid(message)] = ops.as_slice() else {
                panic!("{source}: {ops:?}")
            };
            assert!(message.starts_with("eitri.keymap."), "{source}: {message}");
            assert!(Keymap::apply_user(&ops, &[]).is_err());
        }
        let ops = run("eitri.keymap.set('prefix', 'g', 'zoom', { cells = {} })");
        let KeymapOp::Set { opts, .. } = &ops[0] else { panic!() };
        assert_eq!(
            opts[0].1,
            OptValue::Other("table"),
            "a value of a type no option takes is kept for the error"
        );
    }

    /// Task 3: `eitri.keymap.set("panel", ...)` is recorded the same way as the prefix table, and
    /// `Keymap::apply_user` accepts it.
    #[test]
    fn a_panel_table_set_is_recorded_and_accepted() {
        let ops = run(r#"eitri.keymap.set("panel", "<leader>tn", "tab.new")"#);
        let KeymapOp::Set {
            table,
            key,
            action,
            opts,
        } = &ops[0]
        else {
            panic!("{:?}", ops[0])
        };
        assert_eq!(
            (table.as_str(), key.as_str(), action.as_str(), opts.as_slice()),
            ("panel", "<leader>tn", "tab.new", [].as_slice())
        );
        let map = Keymap::apply_user(&ops, &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(map.panel_user().sets.len(), 1);
    }

    /// Spec §2.5/§2.9: the shipped snippet applies cleanly through real Lua, and does what it says.
    #[test]
    fn the_owners_snippet_applies_cleanly_and_reproduces_his_tmux() {
        let map = Keymap::apply_user(&run(SNIPPET), &[]).unwrap_or_else(|e| panic!("{e}"));
        let k = |s: &str| KeySpec::parse(s).unwrap();
        let action = |s: &str| map.lookup(&k(s)).map(|b| (b.action.clone(), b.repeatable));
        assert_eq!(map.prefix(), &k("C-a"));
        assert_eq!(action("C-a"), Some((Action::SendPrefix, false)));
        assert_eq!(action("C-b"), None);
        assert_eq!(action("%"), None);
        assert_eq!(action("\""), None);
        assert_eq!(action("\\"), Some((Action::Split(Axis::Row), false)));
        assert_eq!(action("-"), Some((Action::Split(Axis::Column), false)));
        assert_eq!(action("|"), Some((Action::Even(Axis::Row), false)));
        assert_eq!(action("_"), Some((Action::Even(Axis::Column), false)));
        assert_eq!(
            action("h"),
            Some((
                Action::Resize {
                    dir: Direction::Left,
                    cells: 5
                },
                true
            ))
        );
        assert_eq!(
            action("l"),
            Some((
                Action::Resize {
                    dir: Direction::Right,
                    cells: 5
                },
                true
            ))
        );
        assert_eq!(action("m"), Some((Action::Zoom, true)));
        assert_eq!(
            action("L"),
            Some((Action::Swap(SwapTarget::Toward(Direction::Right)), false))
        );
        assert_eq!(action("q"), Some((Action::Tab(TabAction::Close), false)));
        assert_eq!(
            action("x"),
            Some((Action::ModuleKill, false)),
            "base.conf:69 (`bind x kill-pane`) is a default"
        );
        assert_eq!(
            action("C-l"),
            Some((Action::SendKeys(k("C-l")), false)),
            "base.conf:73 is a default"
        );
    }

    /// Review Focus 3: the snippet's `pairs` loops run in an order Lua does not specify.
    #[test]
    fn the_snippet_applies_the_same_whatever_order_pairs_runs_in() {
        let ops = run(SNIPPET);
        let forwards = Keymap::apply_user(&ops, &[]).unwrap();
        // Reverse only the eight `set`s the two loops produce, which sit together in the recording.
        let mut shuffled = ops.clone();
        let loop_start = ops
            .iter()
            .position(|op| matches!(op, KeymapOp::Set { action, .. } if action.starts_with("resize.")))
            .unwrap();
        shuffled[loop_start..loop_start + 4].reverse();
        let swaps = ops
            .iter()
            .position(|op| matches!(op, KeymapOp::Set { action, .. } if action.starts_with("swap.")))
            .unwrap();
        shuffled[swaps..swaps + 4].reverse();
        let backwards = Keymap::apply_user(&shuffled, &[]).unwrap();
        for binding in forwards.bindings() {
            assert_eq!(
                backwards.lookup(&binding.key).map(|b| &b.action),
                Some(&binding.action),
                "{}",
                binding.key
            );
        }
        assert_eq!(forwards.bindings().len(), backwards.bindings().len());
    }
}
